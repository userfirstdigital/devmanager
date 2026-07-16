# Browser Task 5A loopback fixture

Serve this directory on a loopback interface only, for example from the repository root:

```powershell
python -m http.server 4173 --bind 127.0.0.1 --directory tests/fixtures/browser-site
```

The site is intentionally self-contained. Missing JSON routes produce failed fetch/XHR traffic; the other static routes cover semantic actions, form/password redaction, delayed mutation, redirects, popups/new tabs, runtime errors, upload, and download behavior.
