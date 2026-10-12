# Security

Ochre opens PDFs, which can come from anywhere, so problems such as a crafted
PDF that makes Ochre crash, run code or write outside the file being saved are
treated as security issues.

## Reporting

Please report them privately through
[GitHub's private reporting](https://github.com/wanikhawar/ochre/security/advisories/new),
not in a public issue. Include the Ochre version and, if you can, a file that
shows the problem.

Fixes go into the latest release only.

Rendering is done by [pdfium](https://pdfium.googlesource.com/pdfium/). A
problem that is in pdfium itself is fixed by updating the bundled pdfium, and
is best also reported
[to pdfium](https://pdfium.googlesource.com/pdfium/+/refs/heads/main/README.md).
