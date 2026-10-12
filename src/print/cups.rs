//! The handful of libcups calls we need. libcups is loaded on first use rather
//! than linked, so Ochre still starts on a system without CUPS; printing then
//! says what's missing.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};

use libloading::Library;

use crate::print::ppd::{self, PrinterOption};

/// Print Studio's virtual printer: sending jobs there from here would go round in a circle.
pub const VIRTUAL_URI_SCHEME: &str = "printstudio:";

#[repr(C)]
struct CupsOption {
    name: *mut c_char,
    value: *mut c_char,
}

#[repr(C)]
struct CupsDest {
    name: *mut c_char,
    instance: *mut c_char,
    is_default: c_int,
    num_options: c_int,
    options: *mut CupsOption,
}

#[repr(C)]
struct CupsJob {
    id: c_int,
    dest: *mut c_char,
    title: *mut c_char,
    user: *mut c_char,
    format: *mut c_char,
    state: c_int,
    size: c_int,
    priority: c_int,
    completed_time: c_long,
    creation_time: c_long,
    processing_time: c_long,
}

const CUPS_WHICHJOBS_ALL: c_int = -1;
const HTTP_STATUS_CONTINUE: c_int = 100;
/// IPP status codes below this are successes.
const IPP_STATUS_ERROR_BAD_REQUEST: c_int = 0x0400;

/// A PDF that's already laid out on the paper. CUPS then skips its pdftopdf
/// stage (which, in libcupsfilters 2.x, drops form XObjects from pages and
/// prints them blank) and goes straight to the printer driver.
pub const FORMAT_PRINT_READY_PDF: &str = "application/vnd.cups-pdf";

/// `CUPS_HTTP_DEFAULT`: the default connection to the CUPS server.
const HTTP_DEFAULT: *mut c_void = ptr::null_mut();

struct Cups {
    get_dests: unsafe extern "C" fn(*mut *mut CupsDest) -> c_int,
    free_dests: unsafe extern "C" fn(c_int, *mut CupsDest),
    get_default: unsafe extern "C" fn() -> *const c_char,
    get_ppd: unsafe extern "C" fn(*const c_char) -> *const c_char,
    add_option: unsafe extern "C" fn(*const c_char, *const c_char, c_int, *mut *mut CupsOption) -> c_int,
    free_options: unsafe extern "C" fn(c_int, *mut CupsOption),
    print_file: unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, c_int, *mut CupsOption) -> c_int,
    last_error_string: unsafe extern "C" fn() -> *const c_char,
    get_jobs: unsafe extern "C" fn(*mut *mut CupsJob, *const c_char, c_int, c_int) -> c_int,
    free_jobs: unsafe extern "C" fn(c_int, *mut CupsJob),
    create_job: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int, *mut CupsOption) -> c_int,
    start_document:
        unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_char, *const c_char, c_int) -> c_int,
    write_request_data: unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> c_int,
    finish_document: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    cancel_job: unsafe extern "C" fn(*const c_char, c_int) -> c_int,
}

/// libcups, loaded once.
fn cups() -> Result<&'static Cups, String> {
    static CUPS: OnceLock<Result<Cups, String>> = OnceLock::new();
    CUPS.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Whether printing can work at all, and if not, why.
pub fn available() -> Result<(), String> {
    cups().map(|_| ())
}

fn load() -> Result<Cups, String> {
    let library = ["libcups.so.2", "libcups.so"]
        .into_iter()
        // SAFETY: libcups has no initialisers with special requirements.
        .find_map(|name| unsafe { Library::new(name) }.ok())
        .ok_or("Printing needs CUPS, which isn't installed. Install your system's cups package, then restart Ochre.")?;
    // Kept for the life of the process: the functions below point into it.
    let library: &'static Library = Box::leak(Box::new(library));
    macro_rules! get {
        ($name:literal) => {
            // SAFETY: the signature matches the libcups 2.x API for this symbol.
            *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                .map_err(|e| format!("This version of CUPS can't be used ({}: {e})", $name))?
        };
    }
    Ok(Cups {
        get_dests: get!("cupsGetDests"),
        free_dests: get!("cupsFreeDests"),
        get_default: get!("cupsGetDefault"),
        get_ppd: get!("cupsGetPPD"),
        add_option: get!("cupsAddOption"),
        free_options: get!("cupsFreeOptions"),
        print_file: get!("cupsPrintFile"),
        last_error_string: get!("cupsLastErrorString"),
        get_jobs: get!("cupsGetJobs"),
        free_jobs: get!("cupsFreeJobs"),
        create_job: get!("cupsCreateJob"),
        start_document: get!("cupsStartDocument"),
        write_request_data: get!("cupsWriteRequestData"),
        finish_document: get!("cupsFinishDocument"),
        cancel_job: get!("cupsCancelJob"),
    })
}

fn string(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

fn last_error(cups: &Cups) -> String {
    string(unsafe { (cups.last_error_string)() }).unwrap_or_else(|| "unknown CUPS error".into())
}

struct Dest {
    name: String,
    is_default: bool,
    options: HashMap<String, String>,
}

/// Asking CUPS for its destinations takes ~0.2 s (it also browses the
/// network), so the answer is kept until [`refresh`].
static DESTS: Mutex<Option<Arc<Vec<Dest>>>> = Mutex::new(None);

fn dests(cups: &Cups) -> Arc<Vec<Dest>> {
    let mut cached = DESTS.lock().unwrap_or_else(|e| e.into_inner());
    cached.get_or_insert_with(|| Arc::new(load_dests(cups))).clone()
}

/// Forget the printers seen so far (one may have been added since).
pub fn refresh() {
    *DESTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn load_dests(cups: &Cups) -> Vec<Dest> {
    let mut raw: *mut CupsDest = ptr::null_mut();
    let n = unsafe { (cups.get_dests)(&mut raw) };
    let mut out = Vec::new();
    for i in 0..n.max(0) as usize {
        let d = unsafe { &*raw.add(i) };
        if !d.instance.is_null() {
            continue; // lpoptions instances like "Printer/draft"
        }
        let mut options = HashMap::new();
        for j in 0..d.num_options.max(0) as usize {
            let o = unsafe { &*d.options.add(j) };
            if let (Some(k), Some(v)) = (string(o.name), string(o.value)) {
                options.insert(k, v);
            }
        }
        if let Some(name) = string(d.name) {
            out.push(Dest { name, is_default: d.is_default != 0, options });
        }
    }
    unsafe { (cups.free_dests)(n, raw) };
    out
}

/// Real printers only: Print Studio's virtual queue is left out.
pub fn printers() -> Result<Vec<String>, String> {
    let cups = cups()?;
    let mut names: Vec<String> = dests(cups)
        .iter()
        .filter(|d| !d.options.get("device-uri").is_some_and(|u| u.starts_with(VIRTUAL_URI_SCHEME)))
        .map(|d| d.name.clone())
        .collect();
    names.sort();
    Ok(names)
}

pub fn default_printer(printers: &[String]) -> Option<String> {
    let cups = cups().ok()?;
    let user_default = dests(cups).iter().find(|d| d.is_default).map(|d| d.name.clone());
    let server_default = string(unsafe { (cups.get_default)() });
    [user_default, server_default]
        .into_iter()
        .flatten()
        .find(|n| printers.contains(n))
        .or_else(|| printers.first().cloned())
}

#[derive(Clone, Debug, Default)]
pub struct PrinterInfo {
    pub name: String,
    pub options: Vec<PrinterOption>,
    pub paper_sizes: HashMap<String, (f64, f64)>,
    pub imageable: HashMap<String, [f64; 4]>,
}

/// Used when the printer doesn't list its printable area (about 1/8 inch).
const DEFAULT_MARGIN: f64 = 9.0;

impl PrinterInfo {
    pub fn paper(&self, page_size: Option<&str>) -> Option<(f64, f64)> {
        let name = page_size?;
        self.paper_sizes.get(name).copied().or_else(|| fallback_paper(name))
    }

    /// The widest unprintable border for a paper size, used on every side
    /// (the paper may go through the printer either way round).
    pub fn margin(&self, page_size: Option<&str>) -> f64 {
        let (Some(name), Some((w, h))) = (page_size, self.paper(page_size)) else { return DEFAULT_MARGIN };
        match self.imageable.get(name) {
            Some([l, b, r, t]) => [*l, *b, w - r, h - t].into_iter().fold(0.0, f64::max).clamp(0.0, 72.0),
            None => DEFAULT_MARGIN,
        }
    }
}

fn fallback_paper(name: &str) -> Option<(f64, f64)> {
    Some(match name {
        "A4" => (595.0, 842.0),
        "A5" => (420.0, 595.0),
        "A6" => (297.0, 420.0),
        "Letter" => (612.0, 792.0),
        "Legal" => (612.0, 1008.0),
        "Executive" => (522.0, 756.0),
        _ => return None,
    })
}

/// The printer's options and paper sizes, from its PPD. Slow (it asks the CUPS
/// server), so call it off the UI thread.
pub fn printer_info(name: &str) -> PrinterInfo {
    let mut info = PrinterInfo { name: name.into(), ..Default::default() };
    let Ok(cups) = cups() else { return info };
    let Some(path) = string(unsafe { (cups.get_ppd)(c(name).as_ptr()) }) else {
        return info; // driverless queue without a PPD
    };
    let text = std::fs::read(&path).map(|b| ppd::decode(&b)).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    let parsed = ppd::parse(&text);
    // Options saved with `lpoptions -p NAME -o key=value` override the PPD defaults.
    let saved = dests(cups).iter().find(|d| d.name == name).map(|d| d.options.clone()).unwrap_or_default();
    info.options = parsed.options;
    for opt in &mut info.options {
        if let Some(v) = saved.get(&opt.keyword).filter(|v| opt.choices.iter().any(|(c, _)| c == *v)) {
            opt.default = v.clone();
        }
    }
    info.paper_sizes = parsed.paper_sizes;
    info.imageable = parsed.imageable;
    info
}

/// Submit a file. With `format` the document type is given explicitly
/// (e.g. `FORMAT_PRINT_READY_PDF`); without it CUPS works it out.
pub fn print_file(
    printer: &str,
    path: &Path,
    title: &str,
    options: &[(String, String)],
    format: Option<&str>,
) -> Result<i32, String> {
    let cups = cups()?;
    let mut opts: *mut CupsOption = ptr::null_mut();
    let mut n: c_int = 0;
    for (k, v) in options {
        n = unsafe { (cups.add_option)(c(k).as_ptr(), c(v).as_ptr(), n, &mut opts) };
    }
    let result = match format {
        None => {
            let file = c(&path.to_string_lossy());
            let id = unsafe { (cups.print_file)(c(printer).as_ptr(), file.as_ptr(), c(title).as_ptr(), n, opts) };
            if id == 0 { Err(last_error(cups)) } else { Ok(id) }
        }
        Some(format) => submit_with_format(cups, printer, path, title, n, opts, format),
    };
    unsafe { (cups.free_options)(n, opts) };
    result
}

fn submit_with_format(
    cups: &Cups,
    printer: &str,
    path: &Path,
    title: &str,
    n: c_int,
    opts: *mut CupsOption,
    format: &str,
) -> Result<i32, String> {
    let data = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let (name, title, format) = (c(printer), c(title), c(format));
    let id = unsafe { (cups.create_job)(HTTP_DEFAULT, name.as_ptr(), title.as_ptr(), n, opts) };
    if id == 0 {
        return Err(last_error(cups));
    }
    let fail = |what: &str| {
        let err = format!("{what}: {}", last_error(cups));
        unsafe { (cups.cancel_job)(name.as_ptr(), id) };
        Err(err)
    };
    let started = unsafe { (cups.start_document)(HTTP_DEFAULT, name.as_ptr(), id, title.as_ptr(), format.as_ptr(), 1) };
    if started != HTTP_STATUS_CONTINUE {
        return fail("Couldn't start the print job");
    }
    for chunk in data.chunks(64 * 1024) {
        if unsafe { (cups.write_request_data)(HTTP_DEFAULT, chunk.as_ptr().cast(), chunk.len()) }
            != HTTP_STATUS_CONTINUE
        {
            return fail("Couldn't send the document");
        }
    }
    if unsafe { (cups.finish_document)(HTTP_DEFAULT, name.as_ptr()) } >= IPP_STATUS_ERROR_BAD_REQUEST {
        return fail("The printer didn't accept the document");
    }
    Ok(id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Held,
    Printing,
    Stopped,
    Cancelled,
    Aborted,
    Done,
    Unknown,
}

impl JobState {
    pub fn is_final(self) -> bool {
        matches!(self, JobState::Cancelled | JobState::Aborted | JobState::Done)
    }

    pub fn describe(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Held => "held",
            JobState::Printing => "printing",
            JobState::Stopped => "stopped",
            JobState::Cancelled => "cancelled",
            JobState::Aborted => "aborted",
            JobState::Done => "done",
            JobState::Unknown => "unknown",
        }
    }
}

pub fn job_state(job_id: i32) -> JobState {
    let Ok(cups) = cups() else { return JobState::Unknown };
    let mut raw: *mut CupsJob = ptr::null_mut();
    let n = unsafe { (cups.get_jobs)(&mut raw, ptr::null(), 1, CUPS_WHICHJOBS_ALL) };
    let mut state = JobState::Unknown;
    for i in 0..n.max(0) as usize {
        let job = unsafe { &*raw.add(i) };
        if job.id == job_id {
            state = match job.state {
                3 => JobState::Queued,
                4 => JobState::Held,
                5 => JobState::Printing,
                6 => JobState::Stopped,
                7 => JobState::Cancelled,
                8 => JobState::Aborted,
                9 => JobState::Done,
                _ => JobState::Unknown,
            };
        }
    }
    unsafe { (cups.free_jobs)(n, raw) };
    state
}
