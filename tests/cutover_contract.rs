#![cfg(windows)]

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

const AUDIT_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/native-next/Invoke-CutoverAudit.ps1"
);
const FIXTURE_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cutover-contract"
);
const FIXTURE_PARITY_SOURCE: &[u8] = b"fn fixture_parity() {}\n";
const FIXTURE_PARITY_SHA256: &str =
    "10f605c7336736cd83db7782a20ee720e4c963befdd96c2447aaef83fb0e8750";

fn read_source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", path.display());
    })
}

struct FixtureRepo {
    _temp: TempDir,
    root: PathBuf,
}
struct AuditRun {
    fixture: FixtureRepo,
    output: Output,
    report: Value,
    human: String,
}

fn base_node(id: &str, kind: &str, status: &str) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "status": status,
        "dependsOn": [],
        "evidence": [format!("evidence/{id}.json")]
    })
}

fn base_row(
    id: &str,
    legacy_path: &str,
    symbols: &[&str],
    replacement_path: &str,
    prerequisites: &[&str],
    status: &str,
) -> Value {
    json!({
        "id": id,
        "area": "fixture-contract",
        "legacy": {
            "path": legacy_path,
            "symbols": symbols,
            "tokens": []
        },
        "replacementOwner": {
            "path": replacement_path,
            "symbol": "ReplacementFixture"
        },
        "prerequisites": prerequisites,
        "evidence": {
            "commands": [format!("pwsh -NoProfile -File evidence/{id}.ps1")],
            "artifacts": [format!("evidence/{id}.json")]
        },
        "tests": [{
            "kind": "cargo-test",
            "path": "tests/fixture_parity.rs",
            "filter": "fixture_parity",
            "evidence": format!("evidence/{id}.json")
        }],
        "e2eProof": {
            "artifact": format!("evidence/{id}.json"),
            "kind": "phase-gate"
        },
        "productionImpact": {
            "profile": "isolated-fixture",
            "preserves": ["config.json", "remote.json"],
            "neverTouches": ["session.json", "production-profile", "provider-sessions"]
        },
        "deletionSet": [legacy_path],
        "status": status,
        "approvalRequired": true,
        "approvalRequirement": "Explicit Phase 11 cutover approval"
    })
}

fn contract(rows: Vec<Value>, nodes: Vec<Value>) -> Value {
    json!({
        "schemaVersion": 1,
        "contractId": "phase-11.1-cutover",
        "ledgerPath": "docs/replacement-deletion-ledger.md",
        "statusModel": ["HOLD", "READY", "DELETED"],
        "referencePolicy": {
            "trackedUniverse": "git-ls-files",
            "referenceScanner": "rg --fixed-strings --line-number",
            "allowedLedgerSelfReferences": ["docs/replacement-deletion-ledger.md"],
            "protectedFileBasenames": ["session.json"],
            "maxMatchesPerRow": 20
        },
        "prerequisiteNodes": nodes,
        "forbiddenEntrypoints": [
            {
                "id": "legacy-devmanager-next",
                "path": "src/bin/devmanager-next.rs",
                "tokens": ["devmanager-next", "devmanager-next.exe"]
            }
        ],
        "rows": rows
    })
}

fn write_ledger(root: &Path, document: &Value) {
    let body = serde_json::to_string_pretty(document).expect("serialize fixture ledger");
    let ledger = format!(
        "# Replacement Deletion Ledger\n\nThe JSON contract below is canonical for the Phase 11.1 audit.\n\n```json cutover-contract\n{body}\n```\n"
    );
    let path = root.join("docs/replacement-deletion-ledger.md");
    fs::create_dir_all(path.parent().expect("ledger parent")).expect("ledger directory");
    fs::write(path, ledger).expect("fixture ledger");
}

fn fixture_auth_token(root: &Path) -> String {
    fs::read_to_string(root.join(".devmanager-next/audit-fixture.auth"))
        .expect("fixture authority marker")
        .trim()
        .to_owned()
}

fn new_fixture_auth_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("fixture authority entropy");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fixture_repo(document: Value, extra_files: &[(&str, &[u8])]) -> FixtureRepo {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let root = temp.path().to_path_buf();
    let auth_token = new_fixture_auth_token();
    fs::create_dir_all(root.join("docs")).expect("fixture docs");
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::create_dir_all(root.join(".devmanager-next")).expect("fixture audit auth directory");
    fs::write(
        root.join(".devmanager-next/audit-fixture.auth"),
        format!("{auth_token}\n"),
    )
    .expect("fixture audit auth marker");
    write_ledger(&root, &document);

    for name in ["legacy.rs", "replacement.rs", "reference.rs", "README.md"] {
        fs::copy(
            Path::new(FIXTURE_ROOT).join(name),
            root.join(if name == "README.md" {
                "README.md".into()
            } else {
                format!("src/{name}")
            }),
        )
        .expect("copy fixture file");
    }
    fs::create_dir_all(root.join("tests")).expect("fixture tests");
    fs::write(root.join("tests/fixture_parity.rs"), FIXTURE_PARITY_SOURCE)
        .expect("fixture parity source");
    for (name, contents) in extra_files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("extra fixture parent");
        }
        fs::write(path, contents).expect("extra fixture file");
    }

    git(&root, &["init", "--quiet"]);
    git(&root, &["add", "--all"]);
    FixtureRepo { _temp: temp, root }
}

fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(["-C", root.to_str().expect("fixture path utf8")])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn bounded_fixture_tool_path() -> std::ffi::OsString {
    let mut directories = Vec::new();
    let mut push_directory = |path: PathBuf| {
        if !path.is_dir() {
            return;
        }
        let text = path.to_string_lossy();
        if text.len() < 3 || text.as_bytes().get(1) != Some(&b':') || text.as_bytes()[2] != b'\\' {
            return;
        }
        if !directories.iter().any(|existing| existing == &path) {
            directories.push(path);
        }
    };
    for candidate in [
        r"C:\Program Files\Git\cmd",
        r"C:\Program Files\Git\bin",
        r"C:\Program Files\Git\mingw64\bin",
        r"C:\Windows\System32",
    ] {
        push_directory(PathBuf::from(candidate));
    }
    assert!(
        !directories.is_empty(),
        "fixture audits need at least one drive-absolute git directory"
    );
    std::env::join_paths(directories).expect("bounded fixture PATH")
}

fn fixture_path_with_shim(shim_root: &Path) -> std::ffi::OsString {
    let mut directories = vec![shim_root.to_path_buf()];
    directories.extend(std::env::split_paths(&bounded_fixture_tool_path()));
    std::env::join_paths(directories).expect("fixture PATH with shim")
}

fn apply_modify_without_delete_child(path: &Path) {
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-Command",
            "$target = Get-Item -LiteralPath $env:CUTOVER_ACL_TARGET; $acl = New-Object System.Security.AccessControl.DirectorySecurity; $acl.SetAccessRuleProtection($true, $false); $id = [System.Security.Principal.WindowsIdentity]::GetCurrent().User; $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($id, [System.Security.AccessControl.FileSystemRights]::Modify, [System.Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit', [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow); $acl.AddAccessRule($rule); Set-Acl -LiteralPath $target.FullName -AclObject $acl",
        ])
        .env("CUTOVER_ACL_TARGET", path)
        .output()
        .expect("apply Modify-only ACL");
    assert!(
        output.status.success(),
        "Modify-only ACL failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_audit(root: &Path, output_path: &Path) -> Output {
    Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", root.join("protected-appdata"))
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", fixture_auth_token(root))
        .env("PATH", bounded_fixture_tool_path())
        .env_remove("DEVMANAGER_PROFILE")
        .output()
        .expect("spawn cutover audit")
}

fn spawn_audit_with_profile(root: &Path, output_path: &Path, profile: &str) -> Output {
    Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", root.join("protected-appdata"))
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", fixture_auth_token(root))
        .env("PATH", bounded_fixture_tool_path())
        .env("DEVMANAGER_PROFILE", profile)
        .output()
        .expect("spawn cutover audit with profile")
}

fn spawn_audit_with_remote_change(
    root: &Path,
    output_path: &Path,
    remote_change_path: &Path,
    human_report_limit: Option<u32>,
) -> Output {
    let mut command = Command::new("pwsh");
    command
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
            "-RemoteChangeEvidencePath",
            remote_change_path
                .to_str()
                .expect("remote change evidence path utf8"),
        ])
        .env("APPDATA", root.join("protected-appdata"))
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", fixture_auth_token(root))
        .env("PATH", bounded_fixture_tool_path())
        .env_remove("DEVMANAGER_PROFILE");
    if let Some(limit) = human_report_limit {
        command.env("DEVMANAGER_CUTOVER_TEST_HUMAN_BYTES", limit.to_string());
    }
    command
        .output()
        .expect("spawn cutover audit with remote change evidence")
}

fn write_git_probe_shim(shim_root: &Path, _log_path: &Path) -> PathBuf {
    fs::create_dir_all(shim_root).expect("git probe shim directory");
    let source_path = shim_root.join("git-probe.cs");
    let executable_path = shim_root.join("git.exe");
    fs::write(
        &source_path,
        r#"using System;
using System.IO;

public static class Program
{
    public static int Main()
    {
        File.AppendAllText(Environment.GetEnvironmentVariable("GIT_PROBE_LOG"), "called\n");
        return 0;
    }
}
"#,
    )
    .expect("git probe shim source");
    let compile = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$source = Get-Content -Raw -LiteralPath $env:GIT_PROBE_SOURCE; Add-Type -TypeDefinition $source -OutputAssembly $env:GIT_PROBE_EXE -OutputType ConsoleApplication",
        ])
        .env("GIT_PROBE_SOURCE", &source_path)
        .env("GIT_PROBE_EXE", &executable_path)
        .output()
        .expect("compile git probe shim");
    assert!(
        compile.status.success(),
        "compile git probe shim failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    executable_path
}

fn write_git_mode_shim(shim_root: &Path) -> PathBuf {
    fs::create_dir_all(shim_root).expect("git mode shim directory");
    let source_path = shim_root.join("git-mode.cs");
    let executable_path = shim_root.join("git.exe");
    fs::write(
        &source_path,
        r#"using System;
using System.Diagnostics;
using System.IO;

public static class Program
{
    private static void Emit(Stream stream, byte value, int count)
    {
        var buffer = new byte[8192];
        for (var index = 0; index < buffer.Length; index++) buffer[index] = value;
        var remaining = count;
        while (remaining > 0)
        {
            var size = Math.Min(remaining, buffer.Length);
            stream.Write(buffer, 0, size);
            stream.Flush();
            remaining -= size;
        }
    }

    public static int Main(string[] args)
    {
        var isEnumeration = Array.IndexOf(args, "ls-files") >= 0;
        var mode = Environment.GetEnvironmentVariable("GIT_FAKE_MODE");
        if (mode == "root-swap")
        {
            var root = Environment.GetEnvironmentVariable("GIT_FAKE_ROOT");
            var moved = Environment.GetEnvironmentVariable("GIT_FAKE_MOVED_ROOT");
            if (string.IsNullOrEmpty(root) || string.IsNullOrEmpty(moved)) return 19;
            try
            {
                Directory.Move(root, moved);
                Directory.CreateDirectory(Path.Combine(root, ".devmanager-next", "evidence", "current"));
                File.WriteAllText(Path.Combine(root, "replacement-sentinel.txt"), "replacement-tree");
                return 18;
            }
            catch (Exception error)
            {
                File.WriteAllText(Environment.GetEnvironmentVariable("GIT_FAKE_SWAP_LOG"), error.GetType().Name + ":" + error.Message);
                return 20;
            }
        }
        if (isEnumeration && !string.IsNullOrEmpty(mode))
        {
            if (mode == "hang")
            {
                System.Threading.Thread.Sleep(30000);
                return 0;
            }
            if (mode == "stdout-overflow")
            {
                Emit(Console.OpenStandardOutput(), (byte)'x', 5000000);
                System.Threading.Thread.Sleep(30000);
                return 0;
            }
            if (mode == "stderr-overflow")
            {
                Emit(Console.OpenStandardError(), (byte)'x', 5000000);
                System.Threading.Thread.Sleep(30000);
                return 0;
            }
            if (mode == "nonzero")
            {
                Console.Error.Write("GIT_CHILD_SENTINEL");
                return 17;
            }
        }

        var quoted = string.Join(" ", Array.ConvertAll(args, value => "\"" + value.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\""));
        var startInfo = new ProcessStartInfo(Environment.GetEnvironmentVariable("GIT_REAL"), quoted);
        startInfo.UseShellExecute = false;
        using (var process = Process.Start(startInfo))
        {
            process.WaitForExit();
            return process.ExitCode;
        }
    }
}
"#,
    )
    .expect("git mode shim source");
    let compile = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$source = Get-Content -Raw -LiteralPath $env:GIT_MODE_SOURCE; Add-Type -TypeDefinition $source -OutputAssembly $env:GIT_MODE_EXE -OutputType ConsoleApplication",
        ])
        .env("GIT_MODE_SOURCE", &source_path)
        .env("GIT_MODE_EXE", &executable_path)
        .output()
        .expect("compile git mode shim");
    assert!(
        compile.status.success(),
        "compile git mode shim failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    executable_path
}

fn real_git_executable() -> PathBuf {
    let output = Command::new("where")
        .arg("git.exe")
        .output()
        .expect("find git executable");
    assert!(output.status.success(), "where git.exe failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("git.exe path")
}

fn write_rg_shim(shim_root: &Path) -> PathBuf {
    fs::create_dir_all(shim_root).expect("rg shim directory");
    let source_path = shim_root.join("rg-shim.cs");
    let executable_path = shim_root.join("rg.exe");
    fs::write(
        &source_path,
        r#"using System;
using System.Diagnostics;
using System.IO;
using System.Text;

public static class Program
{
    private static string Escape(string value)
    {
        var builder = new StringBuilder();
        foreach (var character in value)
        {
            switch (character)
            {
                case '\\': builder.Append("\\\\"); break;
                case '"': builder.Append("\\\""); break;
                case '\r': builder.Append("\\r"); break;
                case '\n': builder.Append("\\n"); break;
                case '\t': builder.Append("\\t"); break;
                default:
                    if (character < 0x20) builder.AppendFormat("\\u{0:X4}", (int)character);
                    else builder.Append(character);
                    break;
            }
        }
        return builder.ToString();
    }

    private static void SpawnResidue()
    {
        var startInfo = new ProcessStartInfo("pwsh");
        startInfo.UseShellExecute = false;
        startInfo.Arguments = "-NoProfile -Command \"Start-Sleep -Milliseconds 60000; [IO.File]::WriteAllText($env:RG_FAKE_RESIDUE, 'residue')\"";
        var residue = Process.Start(startInfo);
        var pidPath = Environment.GetEnvironmentVariable("RG_FAKE_RESIDUE_PID");
        if (!string.IsNullOrEmpty(pidPath)) File.WriteAllText(pidPath, residue.Id.ToString());
    }

    private static void Emit(Stream stream, byte value, int count)
    {
        var buffer = new byte[8192];
        for (var index = 0; index < buffer.Length; index++) buffer[index] = value;
        var remaining = count;
        while (remaining > 0)
        {
            var size = Math.Min(remaining, buffer.Length);
            stream.Write(buffer, 0, size);
            stream.Flush();
            remaining -= size;
        }
    }

    private static void RunFakeMode(string mode)
    {
        if (mode == "stdin-match")
        {
            using (var input = new StreamReader(Console.OpenStandardInput(), Encoding.UTF8))
            {
                var stdin = input.ReadToEnd();
                if (stdin.IndexOf("original-only", StringComparison.Ordinal) >= 0)
                {
                    Console.WriteLine("{\"type\":\"match\",\"data\":{\"line_number\":1,\"submatches\":[{\"match\":{\"text\":\"original-only\"}}]}}");
                }
            }
            return;
        }
        if (mode == "hang")
        {
            SpawnResidue();
            System.Threading.Thread.Sleep(30000);
            return;
        }
        if (mode == "stdout-overflow")
        {
            SpawnResidue();
            Emit(Console.OpenStandardOutput(), (byte)'x', 400000);
            System.Threading.Thread.Sleep(30000);
            return;
        }
        if (mode == "stderr-overflow")
        {
            SpawnResidue();
            Emit(Console.OpenStandardError(), (byte)'x', 400000);
            System.Threading.Thread.Sleep(30000);
            return;
        }
        if (mode == "line-count-overflow")
        {
            for (var index = 0; index < 4097; index++) Console.WriteLine("x");
            return;
        }
        if (mode == "line-length-overflow")
        {
            Console.WriteLine(new string('x', 32769));
            return;
        }

        var path = Environment.GetEnvironmentVariable("RG_FAKE_TARGET");
        if (string.IsNullOrEmpty(path)) throw new InvalidOperationException("missing fake target");
        if (mode == "junction-swap")
        {
            var outside = Environment.GetEnvironmentVariable("RG_FAKE_OUTSIDE");
            if (string.IsNullOrEmpty(outside)) throw new InvalidOperationException("missing junction target");
            var parent = Path.GetDirectoryName(path);
            var moved = parent + ".cutover-junction-original";
            if (Directory.Exists(moved)) Directory.Delete(moved, true);
            Directory.Move(parent, moved);
            var linkStart = new ProcessStartInfo("cmd.exe", "/c mklink /J \"" + parent + "\" \"" + outside + "\"");
            linkStart.UseShellExecute = false;
            using (var link = Process.Start(linkStart)) link.WaitForExit();
            return;
        }
        var bytes = File.ReadAllBytes(path);
        var before = Encoding.UTF8.GetString(bytes);
        var after = before.Replace("original-only", "replaced-only");
        if (mode == "mutate")
        {
            System.Threading.Thread.Sleep(100);
            File.WriteAllBytes(path, Encoding.UTF8.GetBytes(after));
            return;
        }
        if (mode == "swap")
        {
            var moved = path + ".cutover-swap";
            if (File.Exists(moved)) File.Delete(moved);
            File.Move(path, moved);
            File.WriteAllBytes(path, Encoding.UTF8.GetBytes(after));
            return;
        }
        throw new InvalidOperationException("unknown fake mode");
    }

    public static int Main(string[] args)
    {
        var rawArgs = string.Join(" ", Array.ConvertAll(args, value => "\"" + Escape(value) + "\""));
        var usedStdin = false;
        var rewriteAttempted = false;
        var rewriteSucceeded = false;
        var rewriteSameLength = false;
        string path = null;
        foreach (var argument in args)
        {
            if (argument == "-") usedStdin = true;
            if (Path.IsPathRooted(argument) && File.Exists(argument)) path = argument;
        }
        if (path != null)
        {
            rewriteAttempted = true;
            try
            {
                var encoding = new UTF8Encoding(false, true);
                var before = encoding.GetString(File.ReadAllBytes(path));
                var after = before.Replace("original-only", "replaced-only");
                rewriteSameLength = before.Length == after.Length;
                File.WriteAllBytes(path, encoding.GetBytes(after));
                rewriteSucceeded = true;
            }
            catch
            {
                rewriteSucceeded = false;
            }
        }
        var logLine = "{\"rawArgs\":\"" + Escape(rawArgs) + "\",\"usedStdin\":"
            + (usedStdin ? "true" : "false") + ",\"rewriteAttempted\":"
            + (rewriteAttempted ? "true" : "false") + ",\"rewriteSucceeded\":"
            + (rewriteSucceeded ? "true" : "false") + ",\"rewriteSameLength\":"
            + (rewriteSameLength ? "true" : "false") + "}" + Environment.NewLine;
        File.AppendAllText(Environment.GetEnvironmentVariable("RG_SHIM_LOG"), logLine, new UTF8Encoding(false));

        var fakeMode = Environment.GetEnvironmentVariable("RG_FAKE_MODE");
        if (!string.IsNullOrEmpty(fakeMode))
        {
            RunFakeMode(fakeMode);
            return 0;
        }

        var startInfo = new ProcessStartInfo(Environment.GetEnvironmentVariable("RG_REAL"));
        startInfo.UseShellExecute = false;
        startInfo.Arguments = string.Join(" ", Array.ConvertAll(args, value => "\"" + Escape(value) + "\""));
        using (var process = Process.Start(startInfo))
        {
            process.WaitForExit();
            return process.ExitCode;
        }
    }
}
"#,
    )
    .expect("rg shim source");
    let compile = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$source = Get-Content -Raw -LiteralPath $env:RG_SHIM_SOURCE; Add-Type -TypeDefinition $source -OutputAssembly $env:RG_SHIM_EXE -OutputType ConsoleApplication",
        ])
        .env("RG_SHIM_SOURCE", &source_path)
        .env("RG_SHIM_EXE", &executable_path)
        .output()
        .expect("compile rg shim");
    assert!(
        compile.status.success(),
        "compile rg shim failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    executable_path
}

fn spawn_fake_audit(
    root: &Path,
    output_path: &Path,
    mode: &str,
    target: &Path,
    log: &Path,
    residue: &Path,
    shim_root: &Path,
    outside: Option<&Path>,
) -> (Output, Duration) {
    let isolated_path = fixture_path_with_shim(shim_root);
    let mut command = Command::new("pwsh");
    command
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", root.join("protected-appdata"))
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", fixture_auth_token(root))
        .env("RG_FAKE_MODE", mode)
        .env("RG_FAKE_TARGET", target)
        .env("RG_FAKE_RESIDUE", residue)
        .env("RG_FAKE_RESIDUE_PID", residue.with_extension("pid"))
        .env("RG_SHIM_LOG", log)
        .env("PATH", isolated_path);
    if let Some(outside_path) = outside {
        command.env("RG_FAKE_OUTSIDE", outside_path);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn bounded fake audit");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll bounded fake audit").is_some() {
            let elapsed = started.elapsed();
            return (
                child
                    .wait_with_output()
                    .expect("collect bounded fake audit"),
                elapsed,
            );
        }
        if started.elapsed() > Duration::from_secs(17) {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed out fake audit");
            panic!(
                "fake scanner mode {mode} exceeded the bounded audit deadline\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn force_track(root: &Path, paths: &[&str]) {
    let mut args = vec!["add", "--force", "--"];
    args.extend(paths.iter().copied());
    git(root, &args);
}

fn hide_file(path: &Path) {
    let output = Command::new("attrib")
        .args(["+h", path.to_str().expect("hidden fixture path utf8")])
        .output()
        .expect("spawn attrib");
    assert!(
        output.status.success(),
        "attrib failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_junction(link: &Path, target: &Path) {
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-Command",
            "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:CUTOVER_LINK -Target $env:CUTOVER_TARGET -Force | Out-Null",
        ])
        .env("CUTOVER_LINK", link)
        .env("CUTOVER_TARGET", target)
        .output()
        .expect("spawn junction fixture");
    assert!(
        output.status.success(),
        "junction creation failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn process_exists(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let output = Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
        .expect("query process residue");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

fn run_audit_with_setup<F>(document: Value, extra_files: &[(&str, &[u8])], setup: F) -> AuditRun
where
    F: FnOnce(&Path),
{
    let fixture = fixture_repo(document, extra_files);
    setup(&fixture.root);
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = spawn_audit(&fixture.root, &output_path);
    assert!(
        output_path.is_file(),
        "audit must publish JSON even when it fails\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    let human_path = output_path.with_extension("txt");
    let human = fs::read_to_string(human_path).expect("human audit report");
    AuditRun {
        fixture,
        output,
        report,
        human,
    }
}

fn run_audit(document: Value, extra_files: &[(&str, &[u8])]) -> AuditRun {
    run_audit_with_setup(document, extra_files, |_| {})
}

fn row<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .expect("report rows")
        .iter()
        .find(|candidate| candidate["id"] == id)
        .unwrap_or_else(|| panic!("missing report row {id}: {report}"))
}

fn strings_at<'a>(value: &'a Value, path: &[&str]) -> Vec<&'a str> {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_array()
        .expect("string array")
        .iter()
        .map(|value| value.as_str().expect("string entry"))
        .collect()
}

fn merge_contract(mut document: Value, extra: Value) -> Value {
    let object = document.as_object_mut().expect("contract object");
    for (key, value) in extra.as_object().expect("extra object") {
        object.insert(key.clone(), value.clone());
    }
    document
}

fn current_ledger_text() -> &'static str {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/replacement-deletion-ledger.md"
    ))
}

fn current_contract() -> Value {
    let text = current_ledger_text();
    let start = text
        .find("```json cutover-contract\n")
        .expect("cutover contract fence start")
        + "```json cutover-contract\n".len();
    let rest = &text[start..];
    let end = rest.find("\n```").expect("cutover contract fence end");
    serde_json::from_str(&rest[..end]).expect("canonical cutover contract JSON")
}

fn current_rows() -> Vec<Value> {
    current_contract()["rows"].as_array().expect("rows").clone()
}

fn current_row(id: &str) -> Value {
    current_rows()
        .into_iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("missing ledger row {id}"))
}

fn is_handoff_row(row: &Value) -> bool {
    row.get("cutoverAction").and_then(Value::as_str) == Some("handoff")
}

fn current_legacy_path_present(relative: &str) -> bool {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(directory) = relative.strip_suffix('/') {
        let dir = root.join(directory);
        dir.is_dir()
            && fs::read_dir(&dir)
                .ok()
                .is_some_and(|mut entries| entries.next().is_some())
    } else {
        root.join(relative).is_file()
    }
}

fn current_delete_rows() -> Vec<Value> {
    current_rows()
        .into_iter()
        .filter(|row| !is_handoff_row(row))
        .collect()
}

fn expected_deferred_deletion_paths() -> Vec<String> {
    current_delete_rows()
        .into_iter()
        .filter(|row| row["status"] == "HOLD")
        .map(|row| {
            row["legacy"]["path"]
                .as_str()
                .expect("legacy path")
                .to_owned()
        })
        .collect()
}

fn expected_completed_deletion_ids() -> Vec<String> {
    current_delete_rows()
        .into_iter()
        .filter(|row| row["status"] == "DELETED")
        .map(|row| row["id"].as_str().expect("row id").to_owned())
        .collect()
}

fn strip_parity_verifiable_fields(mut row: Value) -> Value {
    if let Some(object) = row.as_object_mut() {
        object.remove("tests");
        object.remove("e2eProof");
        object.remove("productionImpact");
        object.remove("deletionSet");
    }
    row
}

#[test]
fn parity_row_requires_machine_verifiable_owners_tests_e2e_impact_and_deletion_set() {
    let missing = run_audit(
        contract(
            vec![strip_parity_verifiable_fields(base_row(
                "missing-verifiable-fields",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            ))],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(
        !missing.output.status.success(),
        "incomplete parity rows must not exit green"
    );
    assert_eq!(missing.report["contractStatus"], "HOLD");
    assert!(
        strings_at(
            &row(&missing.report, "missing-verifiable-fields"),
            &["blockers"]
        )
        .iter()
        .any(|blocker| *blocker == "audit[unverified]"),
        "missing tests/e2eProof/productionImpact must stay unverified HOLD: {}",
        missing.report
    );
}

#[test]
fn parity_row_rejects_assumed_partial_or_compile_only_claims() {
    let mut compile_only = base_row(
        "compile-only-claim",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "HOLD",
    );
    compile_only["tests"] = json!(["compile-only cargo check"]);
    compile_only["e2eProof"]["kind"] = json!("assumed");
    compile_only["productionImpact"]["profile"] = json!("partial");

    let run = run_audit(
        contract(
            vec![compile_only],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!run.output.status.success());
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert!(
        strings_at(&run.report, &["contractErrors"])
            .iter()
            .any(|error| *error == "audit[contract_invalid]"),
        "assumed/partial/compile-only claims must fail closed: {}",
        run.report
    );
    assert_eq!(row(&run.report, "compile-only-claim")["status"], "HOLD");
}

#[test]
fn parity_ready_row_rejects_stale_or_compile_only_evidence() {
    let stale = run_audit(
        contract(
            vec![base_row(
                "stale-evidence",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            ("evidence/gate-ready.json", br#"{"ok":true}"#),
            ("evidence/stale-evidence.json", br#"{"status":"stale"}"#),
        ],
    );
    assert!(!stale.output.status.success());
    assert_eq!(row(&stale.report, "stale-evidence")["status"], "READY");
    assert!(
        strings_at(&row(&stale.report, "stale-evidence"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "stale evidence must keep the authored READY row blocked: {}",
        stale.report
    );

    let compile_only = run_audit(
        contract(
            vec![base_row(
                "compile-only-evidence",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            ("evidence/gate-ready.json", br#"{"ok":true}"#),
            (
                "evidence/compile-only-evidence.json",
                br#"{"status":"compile-only"}"#,
            ),
        ],
    );
    assert!(!compile_only.output.status.success());
    assert_eq!(
        row(&compile_only.report, "compile-only-evidence")["status"],
        "READY"
    );
    assert!(
        strings_at(
            &row(&compile_only.report, "compile-only-evidence"),
            &["blockers"]
        )
        .iter()
        .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "compile-only evidence must not turn a READY row green: {}",
        compile_only.report
    );
}

#[test]
fn parity_production_profile_and_impact_fail_closed() {
    let mut production_impact = base_row(
        "production-impact",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "HOLD",
    );
    production_impact["productionImpact"]["profile"] = json!("production");
    let impact = run_audit(
        contract(
            vec![production_impact],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!impact.output.status.success());
    assert!(
        strings_at(&impact.report, &["contractErrors"])
            .iter()
            .any(|error| *error == "audit[contract_invalid]")
            || strings_at(&impact.report, &["blockers"])
                .iter()
                .any(|blocker| *blocker == "audit[production_profile]"),
        "production impact profile must fail closed: {}",
        impact.report
    );

    let document = contract(
        vec![base_row(
            "production-env",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let fixture = fixture_repo(document, &[]);
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = spawn_audit_with_profile(&fixture.root, &output_path, "production");
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    assert!(!output.status.success());
    assert!(
        strings_at(&report, &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[production_profile]"),
        "DEVMANAGER_PROFILE=production must fail closed: {report}"
    );
}

#[test]
fn parity_deleted_row_requires_entire_deletion_set_absent() {
    let mut deleted = base_row(
        "leftover-deletion",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "DELETED",
    );
    deleted["deletionSet"] = json!(["src/legacy.rs", "src/reference.rs"]);
    let run = run_audit(
        contract(
            vec![deleted],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[
            ("evidence/gate-parity.json", br#"{"ok":true}"#),
            ("evidence/leftover-deletion.json", br#"{"ok":true}"#),
        ],
    );
    let report_row = row(&run.report, "leftover-deletion");
    assert!(!run.output.status.success());
    assert_eq!(report_row["status"], "DELETED");
    assert!(
        strings_at(report_row, &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[contract_invalid]"),
        "a leftover deletion-set path must keep DELETED blocked: {}",
        run.report
    );
}

fn recognized_evidence(gate_id: &str, test_id: &str, content_sha256: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "kind": "phase-gate",
        "verdict": "pass",
        "gateId": gate_id,
        "testId": test_id,
        "recipe": test_id,
        "source": {
            "path": "tests/fixture_parity.rs",
            "contentSha256": content_sha256
        },
        "completedAtUtc": "2026-08-11T08:00:00.0000000Z",
        "freshnessSeconds": 315360000
    }))
    .expect("serialize recognized evidence")
}

#[test]
fn parity_empty_or_ok_true_evidence_is_not_successful() {
    for (id, body) in [
        ("empty-object", &b"{}"[..]),
        ("ok-true", &br#"{"ok":true}"#[..]),
        ("status-failed", &br#"{"status":"failed"}"#[..]),
        (
            "unknown-schema",
            &br#"{"schemaVersion":99,"verdict":"pass"}"#[..],
        ),
    ] {
        let run = run_audit(
            contract(
                vec![base_row(
                    id,
                    "src/legacy.rs",
                    &["LegacyFixture"],
                    "src/replacement.rs",
                    &["gate-ready"],
                    "READY",
                )],
                vec![base_node("gate-ready", "gate", "READY")],
            ),
            &[
                ("evidence/gate-ready.json", body),
                (&format!("evidence/{id}.json"), body),
            ],
        );
        assert!(!run.output.status.success(), "{id} must not exit green");
        assert_eq!(row(&run.report, id)["status"], "READY");
        assert!(
            strings_at(&row(&run.report, id), &["blockers"])
                .iter()
                .any(|blocker| *blocker == "audit[evidence_invalid]"),
            "{id} existence is not proof: {}",
            run.report
        );
    }
}

#[test]
fn parity_evidence_requires_identity_attestation_and_freshness() {
    let missing_source = run_audit(
        contract(
            vec![base_row(
                "missing-source",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            (
                "evidence/gate-ready.json",
                br#"{"schemaVersion":1,"kind":"phase-gate","verdict":"pass","gateId":"gate-ready","testId":"missing-source","recipe":"missing-source","completedAtUtc":"2026-08-11T08:00:00.0000000Z","freshnessSeconds":315360000}"#,
            ),
            (
                "evidence/missing-source.json",
                br#"{"schemaVersion":1,"kind":"phase-gate","verdict":"pass","gateId":"gate-ready","testId":"missing-source","recipe":"missing-source","completedAtUtc":"2026-08-11T08:00:00.0000000Z","freshnessSeconds":315360000}"#,
            ),
        ],
    );
    assert!(
        strings_at(
            &row(&missing_source.report, "missing-source"),
            &["blockers"]
        )
        .iter()
        .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "missing source attestation must fail closed: {}",
        missing_source.report
    );

    let mismatched = run_audit(
        contract(
            vec![base_row(
                "mismatched-gate",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            (
                "evidence/gate-ready.json",
                recognized_evidence("other-gate", "mismatched-gate", FIXTURE_PARITY_SHA256)
                    .as_slice(),
            ),
            (
                "evidence/mismatched-gate.json",
                recognized_evidence("other-gate", "mismatched-gate", FIXTURE_PARITY_SHA256)
                    .as_slice(),
            ),
            ("tests/fixture_parity.rs", FIXTURE_PARITY_SOURCE),
        ],
    );
    assert!(
        strings_at(&row(&mismatched.report, "mismatched-gate"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "mismatched gate identity must fail closed: {}",
        mismatched.report
    );

    let duplicate = run_audit(
        contract(
            vec![base_row(
                "duplicate-key",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[(
            "evidence/duplicate-key.json",
            br#"{"schemaVersion":1,"schemaVersion":1,"kind":"phase-gate","verdict":"pass","gateId":"gate-ready","testId":"duplicate-key","recipe":"duplicate-key","source":{"path":"tests/fixture_parity.rs","contentSha256":"10f605c7336736cd83db7782a20ee720e4c963befdd96c2447aaef83fb0e8750"},"completedAtUtc":"2026-08-11T08:00:00.0000000Z","freshnessSeconds":315360000}"#,
        )],
    );
    assert!(
        strings_at(&row(&duplicate.report, "duplicate-key"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "duplicate JSON keys must fail closed: {}",
        duplicate.report
    );
}

#[test]
fn parity_commit_only_source_is_not_attestation() {
    let commit_only = br#"{
        "schemaVersion":1,
        "kind":"phase-gate",
        "verdict":"pass",
        "gateId":"gate-ready",
        "testId":"commit-only",
        "recipe":"commit-only",
        "source":{
            "path":"tests/fixture_parity.rs",
            "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        "completedAtUtc":"2026-08-11T08:00:00.0000000Z",
        "freshnessSeconds":315360000
    }"#;
    let run = run_audit(
        contract(
            vec![base_row(
                "commit-only",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            ("evidence/gate-ready.json", commit_only),
            ("evidence/commit-only.json", commit_only),
            ("tests/fixture_parity.rs", FIXTURE_PARITY_SOURCE),
        ],
    );
    assert!(
        !run.output.status.success(),
        "commit-only attestation must not exit green"
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert_ne!(run.report["contractStatus"], "READY");
    assert!(
        strings_at(&row(&run.report, "commit-only"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "an unbound commit string is not identity attestation: {}",
        run.report
    );
    let artifacts = row(&run.report, "commit-only")["evidence"]["artifacts"]
        .as_array()
        .expect("artifact list");
    assert!(
        artifacts.iter().all(|artifact| artifact["present"] != true),
        "commit-only JSON must not be reported present: {}",
        run.report
    );
}

#[test]
fn parity_ledger_recipe_command_is_not_executed_proof() {
    let run = run_audit(
        contract(
            vec![base_row(
                "recipe-only",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            (
                "evidence/gate-ready.json",
                recognized_evidence("gate-ready", "recipe-only", FIXTURE_PARITY_SHA256).as_slice(),
            ),
            (
                "evidence/recipe-only.json",
                recognized_evidence("gate-ready", "recipe-only", FIXTURE_PARITY_SHA256).as_slice(),
            ),
            ("tests/fixture_parity.rs", FIXTURE_PARITY_SOURCE),
        ],
    );
    assert!(
        !run.output.status.success(),
        "a ledger recipe string must not exit green"
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert_ne!(run.report["contractStatus"], "READY");
    assert!(
        strings_at(&row(&run.report, "recipe-only"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "ledger evidence.commands is not captured execution authority: {}",
        run.report
    );
    let artifacts = row(&run.report, "recipe-only")["evidence"]["artifacts"]
        .as_array()
        .expect("artifact list");
    assert!(
        artifacts.iter().all(|artifact| artifact["present"] != true),
        "recipe-only evidence must stay absent: {}",
        run.report
    );
    let published = serde_json::to_string(&run.report).expect("publish JSON");
    assert!(
        !published.contains("pwsh -NoProfile -File evidence/"),
        "report must not echo ledger recipe commands as proof: {published}"
    );
}

#[test]
fn parity_uncorrelated_execution_is_not_present() {
    let uncorrelated = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "kind": "phase-gate",
        "verdict": "pass",
        "gateId": "gate-ready",
        "testId": "uncorrelated-run",
        "recipe": "uncorrelated-run",
        "source": {
            "path": "tests/fixture_parity.rs",
            "contentSha256": FIXTURE_PARITY_SHA256
        },
        "execution": {
            "command": "pwsh -NoProfile -File evidence/uncorrelated-run.ps1",
            "resultSha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "exitCode": 0,
            "completedAtUtc": "2026-08-11T08:00:00.0000000Z",
            "sourceSha256": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "runSha256": "0000000000000000000000000000000000000000000000000000000000000000"
        },
        "completedAtUtc": "2026-08-11T08:00:00.0000000Z",
        "freshnessSeconds": 315360000
    }))
    .expect("serialize uncorrelated execution");
    let run = run_audit(
        contract(
            vec![base_row(
                "uncorrelated-run",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            ("evidence/gate-ready.json", uncorrelated.as_slice()),
            ("evidence/uncorrelated-run.json", uncorrelated.as_slice()),
            ("tests/fixture_parity.rs", FIXTURE_PARITY_SOURCE),
        ],
    );
    assert!(
        !run.output.status.success(),
        "uncorrelated execution must not exit green"
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert!(
        strings_at(&row(&run.report, "uncorrelated-run"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "command/result/hash/exit/time must share one source/run digest: {}",
        run.report
    );
    let artifacts = row(&run.report, "uncorrelated-run")["evidence"]["artifacts"]
        .as_array()
        .expect("artifact list");
    assert!(
        artifacts.iter().all(|artifact| artifact["present"] != true),
        "uncorrelated execution must not be present: {}",
        run.report
    );
}

#[test]
fn parity_named_test_must_bind_tracked_path_filter_and_evidence() {
    let mut invented = base_row(
        "invented-test",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "HOLD",
    );
    invented["tests"] = json!([{
        "kind": "cargo-test",
        "path": "tests/does_not_exist.rs",
        "filter": "no_such_filter",
        "evidence": "evidence/invented-test.json"
    }]);
    let run = run_audit(
        contract(
            vec![invented],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert!(
        strings_at(&row(&run.report, "invented-test"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[unverified]"),
        "invented test targets must stay unverified HOLD: {}",
        run.report
    );
    let report_text = run.report.to_string();
    assert!(
        !report_text.contains("cargo test") && !report_text.contains("does_not_exist.rs"),
        "report must not echo raw invented commands or untrusted paths: {report_text}"
    );
}

#[test]
fn parity_missing_tests_or_impact_are_unverified_hold() {
    let mut stripped = base_row(
        "unverified-hold",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "HOLD",
    );
    stripped.as_object_mut().unwrap().remove("tests");
    stripped.as_object_mut().unwrap().remove("e2eProof");
    stripped.as_object_mut().unwrap().remove("productionImpact");
    let run = run_audit(
        contract(
            vec![stripped],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert_eq!(row(&run.report, "unverified-hold")["status"], "HOLD");
    assert!(
        strings_at(&row(&run.report, "unverified-hold"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[unverified]"),
        "missing tests/e2e/impact must be an unverified HOLD blocker, not a fabricated claim: {}",
        run.report
    );
}

#[test]
fn parity_directory_owner_uses_prefix_boundary() {
    let present = run_audit(
        contract(
            vec![base_row(
                "state-dir",
                "src/state/",
                &["RuntimeState"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[
            ("src/state/mod.rs", b"mod state\n"),
            ("src/statement.rs", b"not a descendant\n"),
        ],
    );
    assert_eq!(
        row(&present.report, "state-dir")["legacy"]["pathPresent"],
        true
    );
    assert_eq!(row(&present.report, "state-dir")["status"], "HOLD");

    let boundary = run_audit(
        contract(
            vec![base_row(
                "state-boundary",
                "src/state/",
                &["RuntimeState"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("src/statement.rs", b"not a descendant\n")],
    );
    assert_eq!(
        row(&boundary.report, "state-boundary")["legacy"]["pathPresent"],
        false,
        "src/statement.rs must not satisfy src/state/: {}",
        boundary.report
    );

    let file_not_dir = run_audit(
        contract(
            vec![base_row(
                "state-file",
                "src/state/",
                &["RuntimeState"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("src/state", b"file not directory\n")],
    );
    assert_eq!(
        row(&file_not_dir.report, "state-file")["legacy"]["pathPresent"],
        false,
        "a file named src/state must not satisfy directory owner src/state/: {}",
        file_not_dir.report
    );

    let wrong_case = run_audit(
        contract(
            vec![base_row(
                "state-case",
                "src/state/",
                &["RuntimeState"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("src/State/mod.rs", b"wrong case descendant\n")],
    );
    assert_eq!(
        row(&wrong_case.report, "state-case")["legacy"]["pathPresent"],
        false,
        "src/State/mod.rs must not satisfy ordinal directory prefix src/state/: {}",
        wrong_case.report
    );

    let mut deleted = base_row(
        "state-deleted",
        "src/state/",
        &["RuntimeState"],
        "src/replacement.rs",
        &["gate-parity"],
        "DELETED",
    );
    deleted["deletionSet"] = json!(["src/state/"]);
    let leftover = run_audit(
        contract(
            vec![deleted],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[("src/state/mod.rs", b"mod state\n")],
    );
    assert_eq!(row(&leftover.report, "state-deleted")["status"], "DELETED");
    assert!(
        strings_at(&row(&leftover.report, "state-deleted"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[contract_invalid]"),
        "DELETED must require directory descendants absent: {}",
        leftover.report
    );
}

#[test]
fn parity_candidate_scanner_does_not_resolve_rg_from_path_or_where() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Invoke-CutoverInternalReferenceScan")
            || source.contains("internal reference scan"),
        "candidate mode needs a bounded internal scanner"
    );
    assert!(
        !source.to_ascii_lowercase().contains("where.exe")
            && !source.contains("where rg")
            && !source.contains("where git"),
        "audit must not PATH-resolve where/rg"
    );
    assert!(
        !source.contains("Contains('rg')") && !source.contains("Contains(\"rg\")"),
        "bare rg substring classification false-greens messages such as argument-binding failures"
    );
    assert!(
        source.contains("536870912"),
        "directory handles must request GENERIC_EXECUTE/FILE_TRAVERSE for relative evidence opens"
    );
    assert!(
        source.contains("0x001201BF") && !source.contains("0x001201FF"),
        "relative directory opens must not demand FILE_DELETE_CHILD on Modify-only worktrees"
    );
    assert!(
        source.contains("authenticated-fixture") && source.contains("candidate-worktree"),
        "rg shims stay fixture-only"
    );
    assert!(
        source.contains("$script:reportDirectoryHandle")
            && source.contains("Close-CutoverPublicationHandles"),
        "publication handles must stay script-owned and joined on close"
    );
    assert!(
        !source.contains("Contains('identity')")
            && !source.contains("Contains('common')")
            && !source.contains("Contains('content')")
            && !source.contains("Contains('changed')")
            && !source.contains("Contains('row')")
            && !source.contains("Contains('node')"),
        "diagnostic classification must use exact recognized phrases, not broad substrings"
    );
    assert!(
        source.contains("recognizedDiagnosticCategories"),
        "already-redacted audit[category] tokens must pass through an exact recognized-state allowlist"
    );
    assert!(
        source.contains("Copy-CutoverRedactedBlockers")
            && !source.contains("Add-GlobalBlocker \"row '$($model.id)': $blocker\""),
        "row-blocker promotion must copy redacted tokens under the deadline, not re-parse prefixed prose"
    );
    assert!(
        source.contains("ExpectedCommands")
            && source.contains("resultSha256")
            && source.contains("exitCode")
            && source.contains("sourceSha256")
            && source.contains("runSha256")
            && source.contains("sourceVolume")
            && source.contains("sourceIndex")
            && source.contains("0000000000000000000000000000000000000000000000000000000000000000"),
        "accepted evidence must bind command/result/hash/exit/time to the current source identity and reject zero/foreign run digests"
    );
    assert!(
        source.contains("Equals($capturedCommand, $recipe"),
        "a ledger/artifact recipe string must never be accepted as the captured command"
    );
    assert!(
        source.contains("Get-CutoverHandleIdentity -Stream $openedSource.stream")
            && source.contains("Equals($claimedVolume, $sourceVolume")
            && source.contains("Equals($claimedIndex, $sourceIndex"),
        "stale-run replay must fail when the captured volume/index is not the current source identity"
    );
    assert!(
        source.contains("$sourceDigest + \"`n\" + $sourceVolume + \"`n\" + $sourceIndex"),
        "tampering with command/result/exit/time/source/identity must invalidate runSha256"
    );
}

#[test]
fn parity_modify_only_acl_still_publishes_hold() {
    let document = contract(
        vec![base_row(
            "modify-only",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let fixture = fixture_repo(document, &[]);
    let evidence_chain = fixture.root.join(".devmanager-next");
    fs::create_dir_all(evidence_chain.join("evidence/current")).expect("precreate evidence chain");
    apply_modify_without_delete_child(&evidence_chain);
    let output_path = evidence_chain.join("evidence/current/cutover-audit.json");
    let output = spawn_audit(&fixture.root, &output_path);
    assert!(
        output_path.is_file(),
        "Modify-only ACL must still publish a report\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("modify-only JSON"))
        .expect("valid modify-only JSON");
    assert!(!output.status.success(), "Modify-only HOLD must not exit 0");
    assert_eq!(report["contractStatus"], "HOLD");
    assert_ne!(report["contractStatus"], "READY");
}

#[test]
fn parity_vacuous_ready_evidence_cannot_publish_success() {
    let run = run_audit(
        contract(
            vec![base_row(
                "vacuous-ready",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[
            ("evidence/gate-ready.json", b"{}"),
            ("evidence/vacuous-ready.json", br#"{"ok":true}"#),
        ],
    );
    assert!(
        !run.output.status.success(),
        "vacuous evidence must not exit green"
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert_ne!(run.report["contractStatus"], "READY");
    assert_eq!(row(&run.report, "vacuous-ready")["status"], "READY");
    assert!(
        strings_at(&row(&run.report, "vacuous-ready"), &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[evidence_invalid]"),
        "vacuous READY evidence must stay blocked: {}",
        run.report
    );
    let artifacts = row(&run.report, "vacuous-ready")["evidence"]["artifacts"]
        .as_array()
        .expect("artifact list");
    assert!(
        artifacts.iter().all(|artifact| artifact["present"] != true),
        "vacuous JSON must not be reported present: {}",
        run.report
    );
}

#[test]
fn parity_report_omits_raw_commands() {
    let run = run_audit(
        contract(
            vec![base_row(
                "no-raw-command",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let published = format!(
        "{}{}{}",
        run.report,
        run.human,
        String::from_utf8_lossy(&run.output.stdout)
    );
    assert!(
        !published.contains("pwsh -NoProfile -File evidence/")
            && !published.contains("cargo test --test"),
        "published report leaked raw commands: {published}"
    );
}

#[test]
fn fixture_audit_detects_legacy_path_symbol_and_external_references() {
    let document = contract(
        vec![base_row(
            "legacy-fixture",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let run = run_audit(document, &[]);
    let legacy = row(&run.report, "legacy-fixture");

    assert!(
        !run.output.status.success(),
        "HOLD evidence must not be green"
    );
    assert_eq!(legacy["legacy"]["pathPresent"], true);
    assert_eq!(legacy["tests"][0]["path"], "tests/fixture_parity.rs");
    assert_eq!(legacy["tests"][0]["filter"], "fixture_parity");
    assert_eq!(
        legacy["e2eProof"]["artifact"],
        "evidence/legacy-fixture.json"
    );
    assert_eq!(legacy["e2eProof"]["kind"], "phase-gate");
    assert_eq!(legacy["productionImpact"]["profile"], "isolated-fixture");
    assert_eq!(legacy["deletionSet"]["paths"], json!(["src/legacy.rs"]));
    assert!(
        strings_at(legacy, &["references", "path"]).contains(&"src/reference.rs"),
        "missing path reference: {}",
        run.report
    );
    assert!(strings_at(legacy, &["references", "symbol"]).contains(&"src/reference.rs"));
    assert!(!strings_at(legacy, &["references", "path"])
        .contains(&"docs/replacement-deletion-ledger.md"));
    assert!(run.human.contains("legacy-fixture"));
    assert!(run.human.contains("HOLD"));
}

#[test]
fn ledger_parser_rejects_missing_and_duplicate_legacy_paths() {
    let missing = run_audit(
        contract(
            vec![base_row(
                "missing-path",
                "src/missing.rs",
                &["Missing"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!missing.output.status.success());
    assert!(strings_at(&missing.report, &["contractErrors"])
        .iter()
        .any(|error| *error == "audit[contract_invalid]"));

    let duplicate = run_audit(
        contract(
            vec![
                base_row(
                    "first",
                    "src/legacy.rs",
                    &["LegacyFixture"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                ),
                base_row(
                    "second",
                    "src/legacy.rs",
                    &["Another"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                ),
            ],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!duplicate.output.status.success());
    assert!(strings_at(&duplicate.report, &["contractErrors"])
        .iter()
        .any(|error| *error == "audit[contract_invalid]"));
}

#[test]
fn prerequisite_graph_rejects_unknown_and_circular_nodes() {
    let unknown = run_audit(
        contract(
            vec![base_row(
                "unknown-prerequisite",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-does-not-exist"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!unknown.output.status.success());
    assert!(
        strings_at(&unknown.report, &["contractErrors"])
            .iter()
            .any(|error| *error == "audit[contract_invalid]"),
        "unknown report: {}",
        unknown.report
    );

    let mut first = base_node("gate-a", "gate", "READY");
    first["dependsOn"] = json!(["gate-b"]);
    let mut second = base_node("gate-b", "gate", "READY");
    second["dependsOn"] = json!(["gate-a"]);
    let circular = run_audit(
        contract(
            vec![base_row(
                "circular",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-a"],
                "HOLD",
            )],
            vec![first, second],
        ),
        &[],
    );
    assert!(!circular.output.status.success());
    assert!(
        strings_at(&circular.report, &["contractErrors"])
            .iter()
            .any(|error| *error == "audit[contract_invalid]"),
        "circular report: {}",
        circular.report
    );
}

#[test]
fn prerequisite_graph_visits_case_distinct_ids_with_ordinal_state() {
    let distinct = {
        let mut node = base_node("GATE-A", "gate", "HOLD");
        node["dependsOn"] = json!(["gate-missing"]);
        node
    };
    let run = run_audit(
        contract(
            vec![base_row(
                "case-distinct-graph",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-a"],
                "HOLD",
            )],
            vec![base_node("gate-a", "gate", "HOLD"), distinct],
        ),
        &[],
    );

    assert!(strings_at(&run.report, &["contractErrors"])
        .iter()
        .any(|error| *error == "audit[contract_invalid]"));
}

#[test]
fn ready_row_requires_all_prerequisites_and_evidence() {
    let run = run_audit(
        contract(
            vec![base_row(
                "not-ready",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "READY",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let report_row = row(&run.report, "not-ready");
    assert!(!run.output.status.success());
    assert_eq!(report_row["status"], "READY");
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[prerequisite_invalid]"));
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[evidence_invalid]"));
}

#[test]
fn ready_prerequisite_requires_its_evidence_artifact() {
    let run = run_audit(
        contract(
            vec![base_row(
                "ready-row",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[("evidence/ready-row.json", br#"{"ok":true}"#)],
    );
    assert!(!run.output.status.success());
    assert!(strings_at(&run.report, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[evidence_invalid]"));
    assert_eq!(
        row(&run.report, "ready-row")["status"],
        "READY",
        "row status remains the authored state while the audit blocks the cutover"
    );
}

#[test]
fn deleted_row_fails_when_legacy_path_is_still_present() {
    let run = run_audit(
        contract(
            vec![base_row(
                "still-present",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "DELETED",
            )],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[
            ("evidence/gate-parity.json", br#"{"ok":true}"#),
            ("evidence/still-present.json", br#"{"ok":true}"#),
        ],
    );
    let report_row = row(&run.report, "still-present");
    assert!(!run.output.status.success());
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[contract_invalid]"));
}

#[test]
fn stale_devmanager_next_entrypoint_is_reported_from_tracked_fixture() {
    let run = run_audit(
        contract(
            vec![base_row(
                "legacy-entrypoint",
                "src/bin/devmanager-next.rs",
                &["main"],
                "src/main.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[
            ("src/bin/devmanager-next.rs", b"fn main() {}\n"),
            ("Cargo.toml", b"name = \"devmanager-next\"\n"),
        ],
    );
    let report_row = row(&run.report, "legacy-entrypoint");
    assert_eq!(report_row["legacy"]["pathPresent"], true);
    assert!(strings_at(&run.report, &["entrypointFindings"])
        .iter()
        .any(|finding| finding.contains("src/bin/devmanager-next.rs")));
    assert!(strings_at(report_row, &["references", "token"])
        .iter()
        .any(|path| *path == "Cargo.toml"));
}

#[test]
fn forbidden_entrypoint_tokens_are_scoped_to_the_exact_entrypoint_path() {
    let mut document = contract(
        vec![base_row(
            "ordinary-row",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    document["forbiddenEntrypoints"][0]["tokens"] = json!(["devmanager-next", "main"]);

    let run = run_audit(
        document,
        &[
            (".gitignore", b"/.devmanager-next/\n"),
            ("src/bin/devmanager-next.rs", b"fn main() {}\n"),
            ("src/other.rs", b"main devmanager-next\n"),
        ],
    );
    let findings = strings_at(&run.report, &["entrypointFindings"]);
    assert!(findings
        .iter()
        .all(|finding| finding.ends_with(":src/bin/devmanager-next.rs")));
    assert!(!findings
        .iter()
        .any(|finding| finding.contains(".gitignore") || finding.contains("src/other.rs")));
}

#[test]
fn tracked_path_presence_requires_the_exact_requested_path() {
    let run = run_audit(
        contract(
            vec![base_row(
                "directory-alias",
                "src/legacy",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("src/legacy/child.rs", b"LegacyFixture\n")],
    );
    assert_eq!(
        row(&run.report, "directory-alias")["legacy"]["pathPresent"],
        false
    );
    assert!(strings_at(&run.report, &["contractErrors"])
        .iter()
        .any(|error| *error == "audit[contract_invalid]"));
}

#[test]
fn ledger_paths_reject_trailing_separators_without_trimming() {
    let rows = [
        ("trailing-slash", "src/legacy.rs/"),
        ("trailing-backslash", r"src\legacy.rs"),
    ]
    .into_iter()
    .map(|(id, path)| {
        base_row(
            id,
            path,
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )
    })
    .collect();
    let run = run_audit(
        contract(rows, vec![base_node("gate-parity", "gate", "HOLD")]),
        &[],
    );

    let errors = strings_at(&run.report, &["contractErrors"]);
    assert!(errors
        .iter()
        .all(|error| *error == "audit[contract_invalid]"));
    assert!(!errors.is_empty());
    assert_eq!(
        row(&run.report, "trailing-slash")["legacy"]["path"],
        "src/legacy.rs/",
        "a single trailing slash is the directory-owner spelling: {}",
        run.report
    );
    assert!(
        row(&run.report, "trailing-backslash")["legacy"]["path"].is_null(),
        "backslashes remain rejected without trimming: {}",
        run.report
    );
}

#[test]
fn bounded_report_fallback_keeps_the_complete_typed_shape() {
    let long_symbols = (0..64)
        .map(|index| format!("symbol-{index}-{}", "x".repeat(500)))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for index in 0..12 {
        let mut row_value = base_row(
            &format!("oversized-{index}"),
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        );
        row_value["legacy"]["symbols"] = json!(long_symbols);
        rows.push(row_value);
    }

    let run = run_audit(
        contract(rows, vec![base_node("gate-parity", "gate", "HOLD")]),
        &[],
    );
    for field in [
        "schemaVersion",
        "contractId",
        "mode",
        "contractStatus",
        "ledgerPath",
        "trackedFileCount",
        "protectedFilesSkipped",
        "contractErrors",
        "blockers",
        "entrypointFindings",
        "prerequisiteNodes",
        "rows",
        "safety",
        "scanner",
    ] {
        assert!(
            run.report.get(field).is_some(),
            "missing report field {field}"
        );
    }
    for field in [
        "protectedFilesSkipped",
        "contractErrors",
        "blockers",
        "entrypointFindings",
        "prerequisiteNodes",
        "rows",
    ] {
        assert!(
            run.report[field].is_array(),
            "report field {field} is not an array"
        );
    }
    assert!(run.human.contains("Phase 11.1 cutover audit"));
    assert!(run.human.contains("status: HOLD"));
}

#[test]
fn oversized_contract_id_keeps_bounded_sanitized_fallback_json() {
    let mut document = contract(
        vec![base_row(
            "oversized-contract-id",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    document["contractId"] = Value::String(format!("contract-\u{1}{}", "x".repeat(300_000)));

    let run = run_audit(document, &[]);
    let output_path = run
        .fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let bytes = fs::read(output_path).expect("bounded fallback JSON");
    assert!(
        bytes.len() <= 262_144,
        "fallback JSON was {} bytes",
        bytes.len()
    );
    let contract_id = run.report["contractId"]
        .as_str()
        .expect("fallback contractId");
    assert_eq!(contract_id, "untrusted-contract-id");
}

#[test]
fn normal_report_sanitizes_and_bounds_contract_id_without_fallback() {
    let mut document = contract(
        vec![base_row(
            "normal-contract-id",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    document["contractId"] = Value::String(format!("contract-\u{1}{}", "x".repeat(300)));

    let run = run_audit(document, &[]);
    let bytes = fs::read(
        run.fixture
            .root
            .join(".devmanager-next/evidence/current/cutover-audit.json"),
    )
    .expect("normal report JSON");
    assert!(
        bytes.len() < 262_144,
        "normal report was {} bytes",
        bytes.len()
    );
    assert_eq!(run.report["safety"]["boundReached"], false);

    let contract_id = run.report["contractId"]
        .as_str()
        .expect("normal contractId");
    assert_eq!(contract_id, "untrusted-contract-id");
}

#[test]
fn oversized_ready_report_propagates_fallback_hold_to_exit() {
    let long_symbols = (0..64)
        .map(|index| format!("symbol-{index}-{}", "x".repeat(500)))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut evidence = vec![(
        "evidence/gate-parity.json".to_string(),
        br#"{"ok":true}"#.to_vec(),
    )];
    for index in 0..12 {
        let id = format!("oversized-ready-{index}");
        let mut row_value = base_row(
            &id,
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "READY",
        );
        row_value["legacy"]["symbols"] = json!(long_symbols.clone());
        rows.push(row_value);
        evidence.push((format!("evidence/{id}.json"), br#"{"ok":true}"#.to_vec()));
    }
    let document = contract(rows, vec![base_node("gate-parity", "gate", "READY")]);
    let evidence_refs = evidence
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect::<Vec<_>>();

    let run = run_audit(document, &evidence_refs);
    assert!(!run.output.status.success());
    assert_eq!(run.report["contractStatus"], "HOLD");
}

#[test]
fn exact_session_json_output_is_rejected_before_any_publish() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "safe-output",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let requested = fixture
        .root
        .join(".devmanager-next/evidence/current/session.json");
    let output = spawn_audit(&fixture.root, &requested);
    let fallback = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    assert!(fallback.is_file(), "safe fallback report must be published");
    assert!(
        !requested.exists(),
        "exact session.json must never be created"
    );
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&fs::read(fallback).expect("fallback JSON"))
        .expect("valid fallback JSON");
    assert!(strings_at(&report, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[output_path_rejected]"));
}

#[test]
fn exact_session_json_is_path_only_and_external_appdata_is_untouched() {
    let session_bytes = br#"{"secret":"must-not-be-read"}"#;
    let run = run_audit(
        contract(
            vec![base_row(
                "session-path",
                "session.json",
                &["session.json"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[
            ("session.json", session_bytes),
            (
                "docs/session-reference.md",
                b"session.json is ignored by the product\n",
            ),
        ],
    );
    let protected = run.fixture.root.join("protected-appdata/session.json");
    fs::create_dir_all(protected.parent().expect("protected parent")).expect("protected dir");
    fs::write(&protected, session_bytes).expect("protected session");

    assert_eq!(
        fs::read(&protected).expect("protected bytes"),
        session_bytes
    );
    assert_eq!(
        fs::read(run.fixture.root.join("session.json")).expect("tracked bytes"),
        session_bytes
    );
    assert!(strings_at(&run.report, &["protectedFilesSkipped"])
        .iter()
        .any(|path| *path == "session.json"));
    assert!(row(&run.report, "session-path")["references"]["path"]
        .as_array()
        .expect("path references")
        .iter()
        .any(|path| path == "docs/session-reference.md"));
    for kind in ["path", "symbol", "token"] {
        assert!(!row(&run.report, "session-path")["references"][kind]
            .as_array()
            .expect("protected reference list")
            .iter()
            .any(|path| path == "session.json"));
    }
    assert!(!run.human.contains("must-not-be-read"));
    assert!(!run.report.to_string().contains("must-not-be-read"));
}

#[test]
fn audit_is_read_only_for_tracked_fixture_files() {
    let document = contract(
        vec![base_row(
            "read-only",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let run = run_audit(document, &[("do-not-delete.txt", b"sentinel\n")]);
    let before = git(&run.fixture.root, &["ls-files"]);
    let before_files = String::from_utf8(before.stdout).expect("tracked paths utf8");
    assert_eq!(
        fs::read(run.fixture.root.join("do-not-delete.txt")).unwrap(),
        b"sentinel\n"
    );
    assert_eq!(
        fs::read(run.fixture.root.join("src/legacy.rs")).unwrap(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cutover-contract/legacy.rs"
        ))
    );
    let after = git(&run.fixture.root, &["ls-files"]);
    assert_eq!(
        String::from_utf8(after.stdout).expect("tracked paths utf8"),
        before_files
    );
    assert!(run.fixture.root.join("do-not-delete.txt").is_file());
}

#[test]
fn tracked_ignored_hidden_binary_and_unicode_names_are_scanned() {
    let composed = "src/unicode-\u{00e9}.txt";
    let decomposed = "src/unicode-e\u{0301}.txt";
    let tabbed = "src/tab\tname.txt";
    let newline = "src/new\nline.txt";
    let extra_files: &[(&str, &[u8])] = &[
        (".gitignore", b"ignored-reference.txt\n"),
        ("ignored-reference.txt", b"ignored-token\n"),
        (".hidden-reference.txt", b"hidden-token\n"),
        ("binary-reference.dat", b"\0binary-token\xff\n"),
        (composed, b"unicode-token\n"),
        (decomposed, b"unicode-token\n"),
    ];
    let mut newline_supported = false;
    let mut tab_supported = false;
    let run = run_audit_with_setup(
        contract(
            vec![base_row(
                "tracked-safety",
                "src/legacy.rs",
                &[
                    "binary-token",
                    "ignored-token",
                    "hidden-token",
                    "unicode-token",
                ],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        extra_files,
        |root| {
            hide_file(&root.join(".hidden-reference.txt"));
            force_track(
                root,
                &[
                    "ignored-reference.txt",
                    ".hidden-reference.txt",
                    "binary-reference.dat",
                    composed,
                    decomposed,
                ],
            );
            if fs::write(root.join(tabbed), b"unicode-token\n").is_ok() {
                tab_supported = true;
                force_track(root, &[tabbed]);
            }
            if fs::write(root.join(newline), b"unicode-token\n").is_ok() {
                newline_supported = true;
                force_track(root, &[newline]);
            }
        },
    );
    let references = strings_at(
        &row(&run.report, "tracked-safety"),
        &["references", "symbol"],
    );
    assert!(references.contains(&"binary-reference.dat"));
    assert!(references.contains(&"ignored-reference.txt"));
    assert!(references.contains(&".hidden-reference.txt"));
    assert!(references.contains(&composed));
    assert!(references.contains(&decomposed));
    if tab_supported {
        assert!(references.contains(&tabbed));
    }
    if newline_supported {
        assert!(references.contains(&newline));
    }
}

#[test]
fn tracked_path_ownership_is_ordinal_and_rejects_case_aliases() {
    let run = run_audit(
        contract(
            vec![base_row(
                "case-alias",
                "SRC/LEGACY.RS",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert_eq!(
        row(&run.report, "case-alias")["legacy"]["pathPresent"],
        false
    );
    assert!(strings_at(&run.report, &["contractErrors"])
        .iter()
        .any(|error| *error == "audit[contract_invalid]"));
}

#[test]
fn ledger_alias_ads_control_and_trailing_space_paths_are_rejected() {
    let invalid = [
        ("dot-segment", "src/./legacy.rs"),
        ("parent-segment", "src/../legacy.rs"),
        ("drive-relative", "C:legacy.rs"),
        ("alternate-stream", "src/legacy.rs:stream"),
        ("control", "src/bad\u{0001}.rs"),
        ("trailing-space", "src/legacy.rs "),
    ];
    let rows = invalid
        .iter()
        .map(|(id, path)| {
            base_row(
                id,
                path,
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )
        })
        .collect();
    let run = run_audit(
        contract(rows, vec![base_node("gate-parity", "gate", "HOLD")]),
        &[],
    );
    let errors = strings_at(&run.report, &["contractErrors"]);
    assert!(!run.output.status.success());
    assert!(
        errors.len() >= invalid.len(),
        "alias errors: {}",
        run.report
    );
    assert!(errors
        .iter()
        .all(|error| *error == "audit[contract_invalid]"));
}

#[test]
fn protected_session_variants_are_not_opened_by_the_scanner() {
    let session_bytes = b"session-exclusive-sentinel\n";
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "session-variant",
                "src/legacy.rs",
                &["session-exclusive-sentinel"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("nested/SESSION.JSON", session_bytes)],
    );
    force_track(&fixture.root, &["nested/SESSION.JSON"]);
    let _exclusive = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(fixture.root.join("nested/SESSION.JSON"))
        .expect("exclusive session fixture handle");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = spawn_audit(&fixture.root, &output_path);
    assert!(
        output_path.is_file(),
        "safe fallback report must be published"
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    assert!(!output.status.success());
    assert!(!strings_at(&report, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("tracked scanner skipped")));
    for kind in ["path", "symbol", "token"] {
        assert!(!row(&report, "session-variant")["references"][kind]
            .as_array()
            .expect("protected reference list")
            .iter()
            .any(|path| path == "nested/SESSION.JSON"));
    }
}

#[test]
fn hardlinks_are_rejected_before_reference_scanning() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "hardlink-row",
                "src/legacy.rs",
                &["hardlink-token"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("hardlink-source.txt", b"hardlink-token\n")],
    );
    let hardlink = fixture.root.join("src/hardlink-reference.txt");
    fs::hard_link(fixture.root.join("hardlink-source.txt"), &hardlink)
        .expect("create hardlink fixture");
    force_track(&fixture.root, &["src/hardlink-reference.txt"]);
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = spawn_audit(&fixture.root, &output_path);
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    assert!(!output.status.success());
    assert!(strings_at(&report, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("hard link") || blocker.contains("hardlink")));
}

#[test]
fn reparse_output_evidence_and_root_attempts_fail_closed() {
    let output_fixture = fixture_repo(
        contract(
            vec![base_row(
                "reparse-output",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let outside_output = output_fixture.root.join("outside-output");
    fs::create_dir_all(&outside_output).expect("outside output directory");
    let evidence = output_fixture.root.join(".devmanager-next/evidence");
    fs::create_dir_all(evidence.parent().expect("evidence parent")).expect("evidence parent");
    create_junction(&evidence, &outside_output);
    let output_path = evidence.join("current/cutover-audit.json");
    let output = spawn_audit(&output_fixture.root, &output_path);
    assert!(!output.status.success());
    assert!(!outside_output.join("current/cutover-audit.json").exists());

    let evidence_fixture = fixture_repo(
        contract(
            vec![base_row(
                "reparse-evidence",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("evidence/ready.json", b"outside-evidence\n")],
    );
    let outside_evidence = evidence_fixture.root.join("outside-evidence");
    fs::create_dir_all(&outside_evidence).expect("outside evidence directory");
    fs::write(outside_evidence.join("ready.json"), b"outside-evidence\n")
        .expect("outside evidence file");
    let evidence_link = evidence_fixture.root.join("evidence");
    fs::remove_dir_all(&evidence_link).expect("remove evidence directory");
    create_junction(&evidence_link, &outside_evidence);
    let evidence_output = evidence_fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let evidence_run = spawn_audit(&evidence_fixture.root, &evidence_output);
    let evidence_report: Value =
        serde_json::from_slice(&fs::read(&evidence_output).expect("safe evidence audit JSON"))
            .expect("valid evidence audit JSON");
    assert!(!evidence_run.status.success());
    assert!(strings_at(&evidence_report, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("reparse") || blocker.contains("evidence")));

    let root_fixture = fixture_repo(
        contract(
            vec![base_row(
                "reparse-root",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let root_alias = root_fixture.root.join("root-alias");
    create_junction(&root_alias, &root_fixture.root);
    let root_output = root_alias.join(".devmanager-next/evidence/current/cutover-audit.json");
    let root_run = spawn_audit(&root_alias, &root_output);
    assert!(!root_run.status.success());
    assert!(!root_output.exists());
}

#[test]
fn ledger_and_report_bounds_stop_collection_with_one_bounded_hold_diagnostic() {
    let huge = "unbounded-token-".repeat(100_000);
    let mut row_value = base_row(
        "oversized-row",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/replacement.rs",
        &["gate-parity"],
        "HOLD",
    );
    row_value["legacy"]["tokens"] = json!([huge]);
    let run = run_audit(
        contract(
            vec![row_value],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let blockers = strings_at(&run.report, &["blockers"]);
    let hold_diagnostics = blockers
        .iter()
        .filter(|blocker| **blocker == "audit[safety_bound]")
        .count();
    assert_eq!(hold_diagnostics, 1);
    assert!(run.report.to_string().len() <= 200_000);
    assert!(run.human.len() <= 100_000);
}

#[test]
fn current_repository_produces_deterministic_hold_report() {
    let output_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let first = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            env!("CARGO_MANIFEST_DIR"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .output()
        .expect("run current audit");
    let first_bytes = fs::read(&output_path).expect("current report");
    let first_report: Value = serde_json::from_slice(&first_bytes).expect("current JSON");
    let second = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            env!("CARGO_MANIFEST_DIR"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .output()
        .expect("rerun current audit");
    let second_bytes = fs::read(&output_path).expect("current report rerun");
    let second_report: Value = serde_json::from_slice(&second_bytes).expect("current JSON rerun");

    assert!(!first.status.success());
    assert!(!second.status.success());
    assert_eq!(first_report, second_report);
    assert_eq!(first_report["contractStatus"], "HOLD");
    let completed = expected_completed_deletion_ids();
    for report_row in first_report["rows"].as_array().expect("current rows") {
        let id = report_row["id"].as_str().expect("report row id");
        if completed.iter().any(|expected| expected == id) {
            assert_eq!(
                report_row["status"], "DELETED",
                "absent deletion-set rows must stay DELETED in the live audit: {id}"
            );
        } else {
            assert_eq!(
                report_row["status"], "HOLD",
                "deferred or handoff rows must stay HOLD until owning-lane evidence: {id}"
            );
        }
    }
    for row in first_report["rows"].as_array().expect("current rows") {
        for kind in ["path", "symbol", "token"] {
            let count = row["references"][kind]
                .as_array()
                .expect("bounded reference list")
                .len();
            assert!(
                count <= 20,
                "reference list for {kind} exceeded bound: {count}"
            );
        }
    }
    assert!(first_report["entrypointFindings"]
        .as_array()
        .is_some_and(|items| items.len() <= 60));
    assert!(first_report["blockers"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));
}

#[test]
fn path_isolated_rg_shim_proves_reference_scan_uses_original_handle_bytes() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "handle-bytes",
                "src/legacy.rs",
                &["original-only"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[("src/race.txt", b"original-only\n")],
    );
    let shim_root = fixture._temp.path().join("rg-shim");
    let shim = write_rg_shim(&shim_root);
    assert!(shim.is_file(), "rg shim must be executable through PATH");
    let log_path = shim_root.join("rg-shim.jsonl");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let isolated_path = fixture_path_with_shim(&shim_root);
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .env(
            "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
            fixture_auth_token(&fixture.root),
        )
        .env("RG_FAKE_MODE", "stdin-match")
        .env("RG_SHIM_LOG", &log_path)
        .env("PATH", isolated_path)
        .output()
        .expect("spawn audit through rg shim");
    assert!(
        output_path.is_file(),
        "audit must publish JSON through the shim\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    let references = strings_at(&row(&report, "handle-bytes"), &["references", "symbol"]);
    assert!(
        references.contains(&"src/race.txt"),
        "the original-only match must survive the shim rewrite attempt: {report}"
    );

    let log = fs::read_to_string(&log_path).unwrap_or_else(|error| {
        panic!(
            "rg shim log: {error:?}\nstdout={}\nstderr={}\nshim={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            shim.display()
        )
    });
    let records = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("rg shim JSON record"))
        .collect::<Vec<_>>();
    assert!(!records.is_empty(), "rg shim must have been invoked");
    let root_text = fixture.root.to_string_lossy().to_ascii_lowercase();
    assert!(
        records.iter().all(|record| record["usedStdin"] == true),
        "every rg invocation must use stdin: {records:?}"
    );
    assert!(
        records.iter().all(|record| {
            !record["rawArgs"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&root_text)
        }),
        "rg must not receive an absolute/raw fixture path: {records:?}"
    );
    assert!(
        records
            .iter()
            .all(|record| record["rewriteAttempted"] == false),
        "the shim's equal-length rewrite path must remain unreachable: {records:?}"
    );
}

#[test]
fn scanner_modes_are_bounded_and_revalidate_content_and_path_identity() {
    let modes = [
        "hang",
        "stdout-overflow",
        "stderr-overflow",
        "line-count-overflow",
        "line-length-overflow",
        "mutate",
        "swap",
    ];
    let mut residue_paths = Vec::new();
    let mut fixtures = Vec::new();

    for mode in modes {
        let fixture = fixture_repo(
            contract(
                vec![base_row(
                    "scanner-safety",
                    "src/legacy.rs",
                    &["original-only"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                )],
                vec![base_node("gate-parity", "gate", "HOLD")],
            ),
            &[("README.md", b"original-only\n")],
        );
        let shim_root = fixture._temp.path().join("rg-shim");
        let shim = write_rg_shim(&shim_root);
        assert!(shim.is_file(), "fake scanner shim must be executable");
        let log_path = shim_root.join("rg-shim.jsonl");
        let residue = shim_root.join("residue.txt");
        let output_path = fixture
            .root
            .join(".devmanager-next/evidence/current/cutover-audit.json");
        let (output, elapsed) = spawn_fake_audit(
            &fixture.root,
            &output_path,
            mode,
            &fixture.root.join("README.md"),
            &log_path,
            &residue,
            &shim_root,
            None,
        );
        assert!(
            elapsed <= Duration::from_millis(16_000),
            "fake scanner mode {mode} took {elapsed:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output_path.is_file(), "mode {mode} must publish a report");
        let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
            .expect("valid audit JSON");
        assert_eq!(report["scanner"]["deadlineMilliseconds"], 15_000);
        assert_eq!(report["scanner"]["maxOutputLines"], 4096);
        assert_eq!(report["scanner"]["maxOutputLineCharacters"], 32768);
        assert!(!output.status.success(), "mode {mode} must fail closed");
        let blockers = strings_at(&report, &["blockers"]);
        match mode {
            "mutate" => assert!(
                blockers
                    .iter()
                    .any(|blocker| *blocker == "audit[file_identity_changed]"),
                "same-length in-place mutation was not rejected: {report}"
            ),
            "swap" => assert!(
                blockers
                    .iter()
                    .any(|blocker| *blocker == "audit[file_identity_changed]"),
                "atomic pathname swap was not rejected: {report}"
            ),
            _ => assert!(
                blockers.iter().any(|blocker| {
                    blocker.starts_with("audit[process_") || *blocker == "audit[safety_bound]"
                }),
                "bounded scanner failure was not reported: {report}"
            ),
        }
        let log = fs::read_to_string(&log_path).expect("fake scanner log");
        assert!(
            log.contains("\"usedStdin\":true"),
            "mode {mode} did not use stdin"
        );
        assert!(
            log.lines()
                .all(|line| !line.contains(&fixture.root.to_string_lossy().to_ascii_lowercase())),
            "mode {mode} received a fixture path in its arguments: {log}"
        );
        let residue_pid_path = shim_root.join("residue.pid");
        if residue_pid_path.is_file() {
            let residue_pid = fs::read_to_string(&residue_pid_path)
                .expect("residue pid")
                .trim()
                .parse::<u32>()
                .expect("numeric residue pid");
            assert!(
                !process_exists(residue_pid),
                "owned descendant survived: {residue_pid}"
            );
        }
        residue_paths.push(residue);
        fixtures.push(fixture);
    }

    thread::sleep(Duration::from_secs(3));
    for residue in residue_paths {
        assert!(
            !residue.exists(),
            "fake scanner residue survived: {}",
            residue.display()
        );
    }
}

#[test]
fn concurrent_junction_path_swap_stays_confined_and_does_not_read_outside() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "junction-safety",
                "src/legacy.rs",
                &["original-only"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let outside = fixture._temp.path().join("outside-junction-target");
    fs::create_dir_all(&outside).expect("outside junction target");
    let outside_sentinel = "JUNCTION_OUTSIDE_SENTINEL";
    fs::write(outside.join("legacy.rs"), outside_sentinel).expect("outside sentinel");

    let shim_root = fixture._temp.path().join("junction-rg-shim");
    let shim = write_rg_shim(&shim_root);
    assert!(shim.is_file());
    let log_path = shim_root.join("rg-shim.jsonl");
    let residue = shim_root.join("residue.txt");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let (output, elapsed) = spawn_fake_audit(
        &fixture.root,
        &output_path,
        "junction-swap",
        &fixture.root.join("src/legacy.rs"),
        &log_path,
        &residue,
        &shim_root,
        Some(&outside),
    );
    assert!(elapsed <= Duration::from_millis(16_000));
    assert!(output_path.is_file(), "junction swap must publish a report");
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("junction JSON"))
        .expect("valid junction JSON");
    assert!(!output.status.success());
    assert!(
        strings_at(&report, &["blockers"])
            .iter()
            .any(|blocker| *blocker == "audit[path_reparse_rejected]"),
        "junction swap blockers: {}\nstderr={}",
        report,
        String::from_utf8_lossy(&output.stderr)
    );
    let human = fs::read_to_string(output_path.with_extension("txt")).expect("junction human");
    for channel in [
        report.to_string(),
        human,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ] {
        assert!(
            !channel.contains(outside_sentinel),
            "outside content leaked: {channel}"
        );
    }
}

#[test]
fn job_owned_descendant_dies_without_touching_unrelated_sentinel() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "job-safety",
                "src/legacy.rs",
                &["original-only"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let shim_root = fixture._temp.path().join("job-rg-shim");
    let shim = write_rg_shim(&shim_root);
    assert!(shim.is_file());
    let log_path = shim_root.join("rg-shim.jsonl");
    let residue = shim_root.join("residue.txt");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let sentinel = fixture._temp.path().join("unrelated-sentinel.txt");
    let mut unrelated = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-Command",
            "Start-Sleep -Milliseconds 750; [IO.File]::WriteAllText($env:UNRELATED_SENTINEL_PATH, 'UNRELATED_SENTINEL')",
        ])
        .env("UNRELATED_SENTINEL_PATH", &sentinel)
        .spawn()
        .expect("spawn unrelated sentinel");
    let (output, elapsed) = spawn_fake_audit(
        &fixture.root,
        &output_path,
        "hang",
        &fixture.root.join("README.md"),
        &log_path,
        &residue,
        &shim_root,
        None,
    );
    unrelated.wait().expect("wait unrelated sentinel");
    assert!(elapsed <= Duration::from_millis(16_000));
    assert!(output_path.is_file(), "job timeout must publish a report");
    assert_eq!(
        fs::read_to_string(&sentinel).expect("unrelated sentinel"),
        "UNRELATED_SENTINEL"
    );
    let residue_pid_path = residue.with_extension("pid");
    let residue_pid = fs::read_to_string(&residue_pid_path)
        .expect("recorded descendant pid")
        .trim()
        .parse::<u32>()
        .expect("descendant pid");
    assert!(
        !process_exists(residue_pid),
        "job descendant survived: {residue_pid}"
    );
    assert!(!output.status.success());
}

#[test]
fn git_enumeration_uses_the_bounded_wrapper_for_all_failure_modes() {
    let real_git = real_git_executable();
    for mode in ["hang", "stdout-overflow", "stderr-overflow", "nonzero"] {
        let fixture = fixture_repo(
            contract(
                vec![base_row(
                    "git-safety",
                    "src/legacy.rs",
                    &["LegacyFixture"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                )],
                vec![base_node("gate-parity", "gate", "HOLD")],
            ),
            &[],
        );
        let shim_root = fixture._temp.path().join("git-mode-shim");
        let shim = write_git_mode_shim(&shim_root);
        assert!(shim.is_file());
        let output_path = fixture
            .root
            .join(".devmanager-next/evidence/current/cutover-audit.json");
        let isolated_path = fixture_path_with_shim(&shim_root);
        let started = Instant::now();
        let output = Command::new("pwsh")
            .args([
                "-NoProfile",
                "-File",
                AUDIT_SCRIPT,
                "-Mode",
                "Parity",
                "-Root",
                fixture.root.to_str().expect("fixture root utf8"),
                "-OutputPath",
                output_path.to_str().expect("output path utf8"),
            ])
            .env("APPDATA", fixture.root.join("protected-appdata"))
            .env(
                "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
                fixture_auth_token(&fixture.root),
            )
            .env("GIT_FAKE_MODE", mode)
            .env("GIT_REAL", &real_git)
            .env("GIT_CHILD_SENTINEL", "GIT_CHILD_SENTINEL")
            .env("PATH", isolated_path)
            .output()
            .expect("spawn git failure audit");
        let elapsed = started.elapsed();
        assert!(
            elapsed <= Duration::from_millis(16_000),
            "git mode {mode} exceeded deadline: {elapsed:?}\\nstdout={}\\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output_path.is_file(),
            "git mode {mode} must publish a report\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("git JSON"))
            .expect("valid git JSON");
        let human = fs::read_to_string(output_path.with_extension("txt")).expect("git human");
        let expected = match mode {
            "hang" => "audit[process_deadline_exceeded]",
            "stdout-overflow" => "audit[process_stdout_overflow]",
            "stderr-overflow" => "audit[process_stderr_overflow]",
            "nonzero" => "audit[process_nonzero]",
            _ => unreachable!(),
        };
        assert!(
            strings_at(&report, &["contractErrors"])
                .iter()
                .chain(strings_at(&report, &["blockers"]).iter())
                .any(|diagnostic| *diagnostic == expected),
            "git mode {mode}: {report}"
        );
        for channel in [
            report.to_string(),
            human,
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ] {
            assert!(
                !channel.contains("GIT_CHILD_SENTINEL"),
                "Git output leaked: {channel}"
            );
        }
    }
}

#[test]
fn retained_root_blocks_replacement_during_git_resolution_and_reports_hold() {
    let real_git = real_git_executable();
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "root-swap",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let swap_container = tempfile::tempdir().expect("root swap container");
    let shim_root = swap_container.path().join("git-root-swap-shim");
    let shim = write_git_mode_shim(&shim_root);
    assert!(shim.is_file());
    let moved_root = swap_container.path().join("authorized-root-moved");
    let swap_log = swap_container.path().join("root-swap.log");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let moved_output = moved_root.join(".devmanager-next/evidence/current/cutover-audit.json");
    let isolated_path = fixture_path_with_shim(&shim_root);
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .env(
            "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
            fixture_auth_token(&fixture.root),
        )
        .env("GIT_FAKE_MODE", "root-swap")
        .env("GIT_FAKE_ROOT", &fixture.root)
        .env("GIT_FAKE_MOVED_ROOT", &moved_root)
        .env("GIT_FAKE_SWAP_LOG", &swap_log)
        .env("GIT_REAL", &real_git)
        .env("PATH", isolated_path)
        .output()
        .expect("spawn root-swap audit");

    assert!(
        !output.status.success(),
        "root replacement must fail closed"
    );
    assert!(
        fixture.root.is_dir() && !moved_root.exists(),
        "the retained authorized root was moved\nstdout={}\nstderr={}\nswap={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(&swap_log).unwrap_or_else(|_| "no swap log".into())
    );
    let swap_failure = fs::read_to_string(&swap_log).expect("root move failure log");
    assert!(
        swap_failure.starts_with("IOException:")
            || swap_failure.starts_with("UnauthorizedAccessException:"),
        "the executable move did not reach the retained-handle sharing guard: {swap_failure}"
    );
    assert!(
        !moved_output.exists() && !moved_output.with_extension("txt").exists(),
        "a nonexistent moved root unexpectedly received a report"
    );
    assert_eq!(
        fixture.root.join("replacement-sentinel.txt").exists(),
        false,
        "the shim created a replacement tree despite the retained root handle"
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("bounded HOLD JSON"))
        .expect("valid bounded HOLD JSON");
    assert_eq!(report["contractStatus"], "HOLD");
    assert!(strings_at(&report, &["contractErrors"]).contains(&"audit[git_identity_invalid]"));
    for current in [
        fixture.root.join(".devmanager-next/evidence/current"),
        moved_root.join(".devmanager-next/evidence/current"),
    ] {
        if !current.is_dir() {
            continue;
        }
        assert!(
            fs::read_dir(&current)
                .expect("current report directory")
                .all(|entry| !entry
                    .expect("current report entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pending-")),
            "root replacement left a pending report in {}",
            current.display()
        );
    }
}

#[test]
fn failed_human_publication_cannot_leave_a_ready_json_report() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "publication-pair",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "READY",
            )],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[
            ("evidence/gate-parity.json", br#"{"ok":true}"#),
            ("evidence/publication-pair.json", br#"{"ok":true}"#),
        ],
    );
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .env(
            "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
            fixture_auth_token(&fixture.root),
        )
        .env("DEVMANAGER_CUTOVER_TEST_FAIL_HUMAN_AFTER_GUARD", "1")
        .output()
        .expect("spawn paired-publication failure audit");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("FIXTURE_HUMAN_PUBLICATION_FAILURE_INJECTED"),
        "the executable human-publication failure was not observed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("guard JSON"))
        .expect("valid guard JSON");
    assert_eq!(report["contractStatus"], "HOLD");
    assert_eq!(report["safety"]["boundReached"], true);
    assert!(strings_at(&report, &["blockers"]).contains(&"audit[safety_bound]"));
}

#[test]
fn failed_final_json_publication_leaves_the_guard_json_on_disk() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "publication-final-json",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "READY",
            )],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[
            ("evidence/gate-parity.json", br#"{"ok":true}"#),
            ("evidence/publication-final-json.json", br#"{"ok":true}"#),
        ],
    );
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .env(
            "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
            fixture_auth_token(&fixture.root),
        )
        .env("DEVMANAGER_CUTOVER_TEST_FAIL_FINAL_JSON_AFTER_HUMAN", "1")
        .output()
        .expect("spawn final-JSON publication failure audit");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("FIXTURE_FINAL_JSON_PUBLICATION_FAILURE_INJECTED"),
        "the executable final-JSON failure was not observed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("guard JSON"))
        .expect("valid guard JSON");
    assert_eq!(report["contractStatus"], "HOLD");
    assert_eq!(report["safety"]["boundReached"], true);
    assert!(strings_at(&report, &["blockers"]).contains(&"audit[safety_bound]"));
    assert!(
        output_path.with_extension("txt").is_file(),
        "the human report should have committed before the injected final-JSON failure"
    );
}

#[test]
fn retained_publication_chain_blocks_root_move_between_report_writes() {
    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "publication-root",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let move_container = tempfile::tempdir().expect("publication move container");
    let moved_root = move_container.path().join("moved-during-publication");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .env(
            "DEVMANAGER_CUTOVER_FIXTURE_AUTH",
            fixture_auth_token(&fixture.root),
        )
        .env("DEVMANAGER_CUTOVER_TEST_MOVE_ROOT_AFTER_GUARD", &moved_root)
        .output()
        .expect("spawn publication root-move audit");

    assert!(!output.status.success(), "fixture contract remains HOLD");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("FIXTURE_PUBLICATION_ROOT_MOVE_BLOCKED"),
        "the executable move attempt was not observed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.is_dir(), "authorized root was moved");
    assert!(!moved_root.exists(), "move target unexpectedly exists");
    assert!(output_path.is_file(), "final JSON report was not published");
    assert!(output_path.with_extension("txt").is_file());
}

#[test]
fn diagnostics_redact_ledger_values_and_absolute_fixture_details_everywhere() {
    let sentinel = "LEDGER_SECRET_SENTINEL";
    let mut document = contract(
        vec![base_row(
            "redaction-row",
            "src/legacy.rs",
            &[sentinel],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    document["contractId"] = Value::String(sentinel.into());
    document["rows"][0]["legacy"]["tokens"] = json!([sentinel]);
    document["rows"][0]["evidence"]["commands"] = json!([sentinel]);
    document["forbiddenEntrypoints"][0]["tokens"] = json!([sentinel]);
    let run = run_audit(document, &[("src/redaction.rs", sentinel.as_bytes())]);
    let json_text = run.report.to_string();
    let human_text = run.human.clone();
    let stdout = String::from_utf8_lossy(&run.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.output.stderr).into_owned();
    let absolute = run.fixture.root.to_string_lossy().to_string();
    for channel in [json_text, human_text, stdout, stderr] {
        assert!(!channel.contains(sentinel), "sentinel leaked: {channel}");
        assert!(
            !channel.contains(&absolute),
            "absolute fixture path leaked: {channel}"
        );
    }
}

#[test]
fn windows_powershell_is_rejected_before_root_access() {
    let temp = tempfile::tempdir().expect("runtime fixture tempdir");
    let root = temp.path().join("runtime-untrusted");
    fs::create_dir_all(&root).expect("runtime fixture root");
    let sentinel = root.join("runtime-sentinel.txt");
    fs::write(&sentinel, "RUNTIME_SENTINEL").expect("runtime sentinel");
    let output_path = root.join(".devmanager-next/evidence/current/report.json");
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("runtime root utf8"),
            "-OutputPath",
            output_path.to_str().expect("runtime output utf8"),
        ])
        .output()
        .expect("spawn Windows PowerShell boundary test");
    assert!(!output.status.success());
    assert!(!output_path.exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported_runtime"),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains("RUNTIME_SENTINEL"));
    assert!(!stderr.contains("RUNTIME_SENTINEL"));
    assert!(sentinel.is_file());
}

#[test]
fn unauthorized_root_is_rejected_before_git_or_fixture_read() {
    let temp = tempfile::tempdir().expect("untrusted temp root");
    let root = temp.path().join("untrusted-repository");
    fs::create_dir_all(&root).expect("untrusted root");
    let sentinel = root.join("source-sentinel.txt");
    let sentinel_text = "UNAUTHORIZED_ROOT_SENTINEL";
    fs::write(&sentinel, sentinel_text).expect("untrusted sentinel");

    let shim_root = temp.path().join("git-probe");
    let probe_log = temp.path().join("git-probe.log");
    let shim = write_git_probe_shim(&shim_root, &probe_log);
    assert!(shim.is_file(), "git probe shim must compile");
    let isolated_path = fixture_path_with_shim(&shim_root);
    let output_path = root.join(".devmanager-next/evidence/current/report.json");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            root.to_str().expect("untrusted root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("PATH", isolated_path)
        .env("GIT_PROBE_LOG", &probe_log)
        .output()
        .expect("spawn unauthorized-root audit");

    assert!(!output.status.success());
    assert!(
        !output_path.exists(),
        "unauthorized root must not receive output"
    );
    assert!(
        !probe_log.exists(),
        "Git must not be started for an unauthorized root"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("root_unauthorized"),
        "missing fixed authorization diagnostic: stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains(sentinel_text));
    assert!(!stderr.contains(sentinel_text));
    assert!(
        sentinel.is_file(),
        "the untrusted sentinel must remain untouched"
    );
}

#[test]
fn process_resolution_uses_canonical_absolute_identity_without_search_path() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Resolve-CutoverExecutable"),
        "Git/RG must be resolved in the parent before child environment isolation"
    );
    assert!(
        source.contains("resolvedExecutable"),
        "the resolved executable identity must flow into process creation"
    );
    assert!(
        !source.contains("SearchPathW"),
        "CreateProcess must not perform ambient SearchPathW substitution"
    );
    assert!(
        source.contains("ValidateExecutableIdentity"),
        "the executable identity must be revalidated at spawn"
    );
}

#[test]
fn unassigned_process_failures_terminate_and_wait_the_root_handle() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("jobAssigned"),
        "cleanup must distinguish assigned and unassigned roots"
    );
    assert!(
        source.contains("TerminateAndWaitRoot"),
        "cleanup must terminate and wait the retained root handle directly"
    );
    assert!(
        source.contains("GetCreationTime") && source.contains("TerminateAndWaitRoot"),
        "creation-time failure must retain a root handle long enough to settle it"
    );
}

#[test]
fn descendant_cleanup_is_deadline_count_and_generation_bounded() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("MaxTrackedProcesses"),
        "descendant enumeration needs an explicit count bound"
    );
    assert!(
        source.contains("CreationTime == parent.CreationTime")
            || source.contains("CreationTime != parent.CreationTime"),
        "parent/child matching must include process creation generation"
    );
    assert!(
        source.contains("Remaining(deadline") || source.contains("Remaining(deadline,"),
        "snapshot and cleanup work must share one absolute deadline"
    );
    assert!(
        !source.contains("for (var pass = 0; pass < 4; pass++)"),
        "fixed repeated PID-only snapshots are not an acceptable cleanup bound"
    );
}

#[test]
fn child_environment_has_bounded_allowlisted_entries_and_secret_rejection() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("MaxEnvironmentEntries") || source.contains("maxEnvironmentEntries"),
        "environment entries need an explicit count bound"
    );
    assert!(
        source.contains("MaxEnvironmentBytes") || source.contains("maxEnvironmentBytes"),
        "the environment block needs an aggregate byte bound"
    );
    assert!(
        source.contains("environment allowlist")
            || source.contains("EnvironmentAllowlist")
            || source.contains("allowedEnvironmentNames"),
        "ambient passthrough must be an explicit canonical allowlist"
    );
    assert!(
        source.contains("SECRET") || source.contains("secret"),
        "secret-shaped environment names must be rejected"
    );
}

#[test]
fn attacker_controlled_git_ledger_and_report_materialization_is_prebounded() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Read-CutoverNulDelimitedPaths"),
        "Git output must be split with a bounded parser"
    );
    assert!(
        source.contains("Read-CutoverContractLines"),
        "ledger lines must be materialized incrementally under a bound"
    );
    assert!(
        source.contains("Assert-CutoverReportBounds"),
        "report serialization must check aggregate shape before ConvertTo-Json"
    );
}

#[test]
fn report_replacement_is_relative_to_verified_parent_handle() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("SetFileInformationByHandle") || source.contains("RenameRelativeToHandle"),
        "final replacement must use a verified parent handle, not an absolute path"
    );
    assert!(
        source.contains("FILE_RENAME_INFO") || source.contains("FileRenameInfo"),
        "replacement must be no-follow and parent-handle-relative"
    );
    assert!(
        !source.contains("[System.IO.File]::Replace($tempPath, $full"),
        "check-then-absolute-path replacement is vulnerable to parent swaps"
    );
}

#[test]
fn report_publication_revalidates_root_even_when_git_identity_is_unavailable() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Assert-CutoverAuthorizedRootIdentityStable"),
        "the opened authorized-root identity needs a Git-independent revalidation helper"
    );
    assert!(
        source.contains("Always revalidate the authorized root before report publication"),
        "Git failure must not bypass root identity revalidation before a HOLD report"
    );
    assert!(
        source.contains("Assert-CutoverPublicationAuthority")
            && source.contains("Open-CutoverRelativeWriteFile")
            && source.contains("NtCreateFile"),
        "publication must bind the retained root/parent identities and create temporary files relative to the verified parent handle"
    );
}

#[test]
fn evidence_directories_are_created_relative_to_retained_no_delete_handles() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Open-CutoverRelativeDirectoryChain")
            && source.contains("CreateOrOpenRelativeDirectory"),
        "evidence directories must be opened or created relative to retained directory handles"
    );
    assert!(
        source.contains("DenyDeleteShare") && source.contains("rootDirectoryHandle"),
        "the root and publication chain must prevent concurrent rename"
    );
    assert!(
        !source.contains("[System.IO.Directory]::CreateDirectory($current)"),
        "path-based check-then-create can follow a concurrently inserted junction"
    );
    assert!(
        source.contains("private const uint FileOpenIf = 3;")
            && source.contains("CreateOrOpenRelativeDirectory"),
        "NtCreateFile FILE_OPEN_IF (disposition 3) must atomically create missing relative directories"
    );

    let fixture = fixture_repo(
        contract(
            vec![base_row(
                "relative-directory-create",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let evidence_root = fixture.root.join(".devmanager-next/evidence");
    assert!(
        !evidence_root.exists(),
        "the fixture must begin without an evidence directory"
    );
    let output_path = evidence_root.join("current/cutover-audit.json");
    let output = spawn_audit(&fixture.root, &output_path);
    assert!(!output.status.success(), "fixture contract remains HOLD");
    let report: Value =
        serde_json::from_slice(&fs::read(&output_path).expect("handle-relative created report"))
            .expect("valid handle-relative created JSON");
    assert_eq!(report["contractStatus"], "HOLD");
    assert!(
        output_path.with_extension("txt").is_file(),
        "the missing evidence/current chain was not created relative to retained handles"
    );
}

#[test]
fn human_report_truncation_is_explicitly_converted_to_bounded_hold() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("humanTruncated") && source.contains("$humanTruncated -or"),
        "the per-line writer must retain evidence that it omitted report content"
    );
    assert!(
        source.contains("humanOmittedLineCount")
            && source.contains("humanReportOmittedLineCount")
            && source.contains("report content omitted due to safety bound"),
        "the bounded HOLD fallback must disclose a bounded omitted-line count and marker"
    );
    assert!(
        source.contains("status: HOLD") && source.contains("$safetyDiagnostic"),
        "a truncated human report must be replaced by an explicit bounded HOLD"
    );
    assert!(
        source.contains("boundedBlockers")
            && source.contains("audit[remote_change_protected]")
            && source.contains("audit[remote_change_unattributed]"),
        "bounded fallback must retain explicit remote-change HOLD blockers"
    );
}

#[test]
fn fixture_authority_is_ephemeral_and_not_a_forgeable_static_marker() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Get-CutoverFixtureAuthority")
            || source.contains("RandomNumberGenerator")
            || source.contains("RandomNumberGenerator.Fill"),
        "fixture authorization must be generated per fixture rather than using a known constant"
    );
    assert!(
        !source.contains("phase-11.1a-generated-fixture-v1"),
        "the audit script must not embed a reusable fixture capability"
    );
    assert!(
        source.contains("fixture authority") && source.contains("one-time"),
        "fixture authorization must document its one-time test-only boundary"
    );
}

#[test]
fn tool_resolution_trusts_pinned_installations_and_only_allows_fixture_shims_in_fixture_mode() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Get-CutoverTrustedExecutable")
            || source.contains("Test-CutoverTrustedExecutablePath"),
        "Git and rg must be selected from trusted pinned locations"
    );
    assert!(
        source.contains("authenticated-fixture") && source.contains("fixture tool"),
        "arbitrary test shims must be confined to the authenticated fixture mode"
    );
    assert!(
        source.contains("trusted tool root") || source.contains("trustedToolRoots"),
        "candidate audits must reject an ambient PATH executable"
    );
}

#[test]
fn candidate_tool_resolution_never_enumerates_ambient_path_or_provider_cache_suffixes() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("Get-CutoverCanonicalInstalledExecutableCandidates"),
        "candidate Git/rg lookup must be generated exclusively from canonical installation roots"
    );
    assert!(
        source.contains("executed image") || source.contains("ValidateExecutableIdentity"),
        "the selected canonical tool image must be attested after CreateProcess"
    );
    assert!(
        !source.contains("\\node_modules\\@openai\\codex-")
            && !source.contains("\\node_modules\\@anthropic-ai\\claude-code"),
        "a package-shaped path supplied through ambient PATH is forgeable"
    );
    assert!(
        source
            .to_ascii_lowercase()
            .contains("candidate audits never enumerate ambient path"),
        "the candidate-mode PATH prohibition must remain explicit"
    );
}

#[test]
fn candidate_child_environment_uses_canonical_os_paths_and_no_ambient_pwsh_lookup() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("SpecialFolder]::Windows")
            && source.contains("[Environment]::SystemDirectory"),
        "candidate Windows paths must come from OS known-folder APIs"
    );
    for ambient in [
        "$env:SystemRoot",
        "$env:WINDIR",
        "$env:ComSpec",
        "Get-Command pwsh",
    ] {
        assert!(
            !source.contains(ambient),
            "candidate child environment still trusts ambient lookup: {ambient}"
        );
    }
    assert!(
        source.contains("fixture-only environment")
            && source.contains("authorizedRootKind -eq 'authenticated-fixture'"),
        "shim controls must only enter authenticated fixture children"
    );
}

#[test]
fn creation_identity_failure_preserves_and_reports_root_cleanup_proof() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("ProcessLaunchException") || source.contains("RootCleanupProven"),
        "a creation-time identity failure must carry the retained root cleanup result"
    );
    assert!(
        source.contains("ActiveProcessZero = launch")
            || source.contains("ActiveProcessZero = processLaunch"),
        "ACTIVE_PROCESS_ZERO must reflect the actual root termination outcome"
    );
    assert!(
        source.contains("TerminateAndWaitRoot") && source.contains("rootCleanupProven"),
        "the retained root handle must be terminated and waited before reporting"
    );
}

#[test]
fn descendant_records_include_pid_creation_and_verified_executable_identity() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("ExecutablePath") && source.contains("ExecutableIdentity"),
        "owned descendants must retain executable identity alongside PID and creation time"
    );
    assert!(
        source.contains("GetProcessImagePath") || source.contains("QueryFullProcessImageNameW"),
        "descendant executable identity must come from the process handle"
    );
    assert!(
        source.contains("OpenExecutableIdentity"),
        "descendant paths must be checked as opened file identities"
    );
}

#[test]
fn descendant_dedupe_uses_pid_creation_path_and_opened_executable_identity() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("SameTrackedProcessIdentity"),
        "descendant dedupe needs one full-identity predicate"
    );
    assert!(
        source.contains("existing.ProcessId")
            && source.contains("existing.CreationTime")
            && source.contains("existing.ExecutablePath")
            && source.contains("existing.ExecutableIdentity")
            && source.contains("SameFileIdentity"),
        "PID and creation time alone are not a complete descendant identity"
    );
}

#[test]
fn scanner_lines_are_count_and_length_bounded_before_collection() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("maxScannerOutputLines") && source.contains("maxScannerOutputLineChars"),
        "scanner output needs explicit line-count and per-line limits"
    );
    assert!(
        source.contains("Read-CutoverUtf8Lines")
            && source.contains("MaxBytes")
            && source.contains("MaxLineChars"),
        "byte, line-count, and line-length bounds must be applied by the incremental parser"
    );
    assert!(
        !source.contains("$stdout -split"),
        "unbounded split materializes attacker-controlled scanner lines before checking limits"
    );
    assert!(
        source.contains("Read-CutoverUtf8Lines")
            && source.contains("Read-CutoverContractLines")
            && source.contains("Assert-CutoverWorkDeadline"),
        "attacker-controlled parsers must preserve the settlement/publication reserve"
    );
}

fn remote_state_fixture(pairing_token: &str, last_seen: u64, activity: Vec<Value>) -> Value {
    json!({
        "host": {
            "enabled": true,
            "bindAddress": "127.0.0.1",
            "port": 43871,
            "keepHostingInBackground": true,
            "serverId": "server-id-must-never-leak",
            "pairingToken": pairing_token,
            "certificatePem": "certificate-must-never-leak",
            "privateKeyPem": "private-key-must-never-leak",
            "certificateFingerprint": "fingerprint-must-never-leak",
            "pairedClients": [],
            "web": {
                "enabled": true,
                "bindAddress": "127.0.0.1",
                "port": 43872,
                "pairingToken": "web-pairing-must-never-leak",
                "cookieSecretHex": "cookie-secret-must-never-leak",
                "pairedClients": [{
                    "clientId": "browser-client-id-must-never-leak",
                    "browserInstallId": "browser-install-id-must-never-leak",
                    "nickname": "private-browser-name-must-never-leak",
                    "label": "private-browser-label-must-never-leak",
                    "issuedAtEpochMs": 10,
                    "lastSeenEpochMs": last_seen,
                    "lastSeenIp": "192.0.2.44",
                    "userAgent": "private-user-agent-must-never-leak",
                    "browserFamily": "Browser",
                    "browserVersion": "1",
                    "osFamily": "OS",
                    "deviceClass": "desktop"
                }],
                "activityLog": activity,
                "push": {
                    "vapidPublicKey": "push-public-must-never-leak",
                    "vapidPrivateKey": "push-private-must-never-leak",
                    "subscriptions": []
                }
            }
        },
        "knownHosts": []
    })
}

fn browser_activity(event_at: u64) -> Value {
    json!({
        "clientId": "browser-client-id-must-never-leak",
        "source": "browser",
        "eventKind": "reconnected",
        "label": "private-browser-label-must-never-leak",
        "ipAddress": "192.0.2.44",
        "eventAtEpochMs": event_at,
        "browserFamily": "Browser",
        "browserVersion": "1",
        "osFamily": "OS",
        "deviceClass": "desktop"
    })
}

fn remote_change_evidence(after_pairing_token: &str) -> Value {
    let first_activity = browser_activity(100);
    json!({
        "schemaVersion": 1,
        "writer": {
            "installedDevManagerImageAttested": true,
            "processIdMatched": true,
            "creationTimeMatched": true
        },
        "before": remote_state_fixture(
            "host-pairing-must-never-leak",
            100,
            vec![first_activity.clone()]
        ),
        "after": remote_state_fixture(
            after_pairing_token,
            200,
            vec![first_activity, browser_activity(200)]
        )
    })
}

fn run_remote_change_evidence_with_human_limit(
    evidence: Value,
    human_report_limit: Option<u32>,
) -> AuditRun {
    let document = contract(
        vec![base_row(
            "remote-attribution",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let evidence_bytes =
        serde_json::to_vec_pretty(&evidence).expect("serialize remote change evidence");
    let fixture = fixture_repo(
        document,
        &[(
            ".devmanager-next/fixtures/remote-change.json",
            evidence_bytes.as_slice(),
        )],
    );
    let evidence_path = fixture
        .root
        .join(".devmanager-next/fixtures/remote-change.json");
    let before_bytes = fs::read(&evidence_path).expect("remote evidence before audit");
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = spawn_audit_with_remote_change(
        &fixture.root,
        &output_path,
        &evidence_path,
        human_report_limit,
    );
    assert!(
        output_path.is_file(),
        "remote attribution audit must publish a report"
    );
    assert_eq!(
        fs::read(&evidence_path).expect("remote evidence after audit"),
        before_bytes,
        "remote attribution must be read-only"
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    let human = fs::read_to_string(output_path.with_extension("txt")).expect("human audit report");
    AuditRun {
        fixture,
        output,
        report,
        human,
    }
}

fn run_remote_change_evidence(evidence: Value) -> AuditRun {
    run_remote_change_evidence_with_human_limit(evidence, None)
}

fn run_remote_change_audit(after_pairing_token: &str) -> AuditRun {
    run_remote_change_evidence(remote_change_evidence(after_pairing_token))
}

#[test]
fn bounded_human_report_emits_omission_count_and_preserves_remote_hold() {
    let run = run_remote_change_evidence_with_human_limit(
        remote_change_evidence("changed-pairing-authority-must-never-leak"),
        Some(256),
    );
    assert_eq!(run.report["contractStatus"], "HOLD");
    assert_eq!(run.report["safety"]["humanReportTruncated"], true);
    let omitted = run.report["safety"]["humanReportOmittedLineCount"]
        .as_u64()
        .expect("human omitted-line count");
    assert!(omitted > 0, "truncation must disclose omitted lines");
    assert!(run
        .human
        .contains("report content omitted due to safety bound"));
    assert!(run.human.contains(&format!("omitted lines: {omitted}")));
    assert!(
        run.human.contains("audit[remote_change_protected]"),
        "the bounded human HOLD must retain its specific safety reason: {}",
        run.human
    );
    assert!(strings_at(&run.report, &["blockers"]).contains(&"audit[remote_change_protected]"));
}

#[test]
fn remote_change_attribution_is_read_only_semantic_and_redacted() {
    let activity = run_remote_change_audit("host-pairing-must-never-leak");
    assert_eq!(
        activity.report["remoteChangeAttribution"]["classification"],
        "authorized-installed-app-browser-activity"
    );
    assert_eq!(
        activity.report["remoteChangeAttribution"]["writer"],
        "verified-installed-app-generation"
    );
    assert_eq!(
        activity.report["remoteChangeAttribution"]["changedCategories"],
        json!(["browser-activity-log", "browser-last-seen"])
    );
    assert!(
        !strings_at(&activity.report, &["blockers"]).contains(&"audit[remote_change_unattributed]")
    );
    assert!(
        !strings_at(&activity.report, &["blockers"]).contains(&"audit[remote_change_protected]")
    );

    let mut paired_event = remote_change_evidence("host-pairing-must-never-leak");
    paired_event["after"]["host"]["web"]["activityLog"][1]["eventKind"] = json!("paired");
    let paired_event = run_remote_change_evidence(paired_event);
    assert_eq!(
        paired_event.report["remoteChangeAttribution"]["classification"],
        "authorized-installed-app-browser-activity",
        "the installed app's paired activity kind is valid log metadata when device authority is unchanged"
    );

    let mut paired_authority = remote_change_evidence("host-pairing-must-never-leak");
    paired_authority["after"]["host"]["web"]["activityLog"][1]["eventKind"] = json!("paired");
    let mut second_client = paired_authority["after"]["host"]["web"]["pairedClients"][0].clone();
    second_client["clientId"] = json!("second-browser-client-id-must-never-leak");
    paired_authority["after"]["host"]["web"]["pairedClients"]
        .as_array_mut()
        .expect("paired client array")
        .push(second_client);
    let paired_authority = run_remote_change_evidence(paired_authority);
    assert_eq!(
        paired_authority.report["remoteChangeAttribution"]["classification"],
        "protected-or-unclassified-change",
        "a real paired-device authority change must remain protected"
    );
    assert!(strings_at(&paired_authority.report, &["blockers"])
        .contains(&"audit[remote_change_protected]"));

    let mut bounded_log = remote_change_evidence("host-pairing-must-never-leak");
    bounded_log["before"]["host"]["web"]["activityLog"] =
        Value::Array((0..100).map(browser_activity).collect());
    bounded_log["after"]["host"]["web"]["activityLog"] =
        Value::Array((1..=100).map(browser_activity).collect());
    let bounded_log = run_remote_change_evidence(bounded_log);
    assert_eq!(
        bounded_log.report["remoteChangeAttribution"]["classification"],
        "authorized-installed-app-browser-activity",
        "one bounded-log trim plus one append is normal installed-app activity"
    );

    let mut short_log_rewrite = remote_change_evidence("host-pairing-must-never-leak");
    short_log_rewrite["before"]["host"]["web"]["activityLog"] =
        Value::Array((0..3).map(browser_activity).collect());
    short_log_rewrite["after"]["host"]["web"]["activityLog"] =
        Value::Array((1..=3).map(browser_activity).collect());
    let short_log_rewrite = run_remote_change_evidence(short_log_rewrite);
    assert_eq!(
        short_log_rewrite.report["remoteChangeAttribution"]["classification"],
        "protected-or-unclassified-change",
        "history may only be trimmed when appends overflow the 100-entry bound"
    );
    assert!(strings_at(&short_log_rewrite.report, &["blockers"])
        .contains(&"audit[remote_change_protected]"));

    let mut unverified = remote_change_evidence("host-pairing-must-never-leak");
    unverified["writer"]["creationTimeMatched"] = Value::Bool(false);
    let unverified = run_remote_change_evidence(unverified);
    assert_eq!(
        unverified.report["remoteChangeAttribution"]["classification"],
        "browser-activity-unattributed"
    );
    assert!(strings_at(&unverified.report, &["blockers"])
        .contains(&"audit[remote_change_unattributed]"));

    let mut protected_evidence =
        remote_change_evidence("changed-pairing-authority-must-never-leak");
    protected_evidence["after"]["host"]["web"]["cookieSecretHex"] =
        json!("changed-cookie-secret-must-never-leak");
    protected_evidence["after"]["knownHosts"] = json!([{
        "serverId": "known-host-id-must-never-leak",
        "pairingToken": "known-host-token-must-never-leak"
    }]);
    let authority = run_remote_change_evidence(protected_evidence);
    assert_eq!(
        authority.report["remoteChangeAttribution"]["classification"],
        "protected-or-unclassified-change"
    );
    assert!(
        authority.report["remoteChangeAttribution"]["changedCategories"]
            .as_array()
            .expect("changed categories")
            .iter()
            .any(|category| category == "protected-or-unclassified")
    );
    assert!(
        strings_at(&authority.report, &["blockers"]).contains(&"audit[remote_change_protected]")
    );

    for run in [
        &activity,
        &paired_event,
        &paired_authority,
        &bounded_log,
        &unverified,
        &authority,
    ] {
        let channels = [
            run.report.to_string(),
            run.human.clone(),
            String::from_utf8_lossy(&run.output.stdout).into_owned(),
            String::from_utf8_lossy(&run.output.stderr).into_owned(),
        ];
        for channel in channels {
            for secret in [
                "server-id-must-never-leak",
                "host-pairing-must-never-leak",
                "changed-pairing-authority-must-never-leak",
                "cookie-secret-must-never-leak",
                "changed-cookie-secret-must-never-leak",
                "known-host-id-must-never-leak",
                "known-host-token-must-never-leak",
                "private-key-must-never-leak",
                "push-private-must-never-leak",
                "browser-client-id-must-never-leak",
                "second-browser-client-id-must-never-leak",
                "browser-install-id-must-never-leak",
                "private-browser-name-must-never-leak",
                "private-user-agent-must-never-leak",
                "192.0.2.44",
            ] {
                assert!(
                    !channel.contains(secret),
                    "remote evidence leaked: {channel}"
                );
            }
        }
    }
}

#[test]
fn temporary_report_creation_records_identity_and_deletes_failed_handles() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("tempIdentity"),
        "temporary report creation must retain the handle identity it created"
    );
    assert!(
        source.contains("DeleteByHandle") && source.contains("Open-CutoverRelativeWriteFile"),
        "failed temporary creation must delete through its retained handle"
    );
    assert!(
        source.contains("tempHandle") && source.contains("finally"),
        "report handles must be closed on every failure path"
    );
}

#[test]
fn scanner_job_has_a_bounded_active_process_limit() {
    let source = fs::read_to_string(AUDIT_SCRIPT).expect("read audit script");
    assert!(
        source.contains("JobObjectLimitActiveProcess"),
        "the scanner Job must enforce an active-process limit"
    );
    assert!(
        source.contains("ActiveProcessLimit") && source.contains("MaxTrackedProcesses"),
        "the active-process limit must be tied to the bounded descendant budget"
    );
}

#[test]
fn handoff_row_missing_replacement_is_blocker_not_contract_error() {
    let mut handoff_row = base_row(
        "handoff-updater",
        "src/legacy.rs",
        &["LegacyFixture"],
        "src/handoff.rs",
        &["gate-parity"],
        "HOLD",
    );
    handoff_row["cutoverAction"] = json!("handoff");
    handoff_row
        .as_object_mut()
        .expect("row object")
        .remove("deletionSet");
    let run = run_audit(
        contract(
            vec![handoff_row],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let report_row = row(&run.report, "handoff-updater");
    assert!(!run.output.status.success());
    assert!(!strings_at(&run.report, &["contractErrors"])
        .iter()
        .any(|error| error.contains("replacement owner path is not an exact tracked path")));
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("handoff replacement")));
    assert_eq!(report_row["cutoverAction"], "handoff");
    assert!(report_row["deletionSet"]["paths"]
        .as_array()
        .expect("handoff deletion paths")
        .is_empty());
}

#[test]
fn entry_product_entrypoints_report_old_app_dispatch() {
    let document = merge_contract(
        contract(
            vec![base_row(
                "legacy-app",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        json!({
            "productEntrypoints": {
                "desktopClient": {
                    "id": "gpui-desktop-client",
                    "path": "src/replacement.rs",
                    "symbol": "main",
                    "role": "gpui-client",
                    "forbiddenDispatch": ["LegacyFixture"]
                },
                "durableHost": {
                    "id": "durable-host",
                    "path": "src/replacement.rs",
                    "symbol": "main",
                    "role": "durable-host",
                    "lifecycle": ["attach", "detach", "full-quit"]
                }
            }
        }),
    );
    let run = run_audit(
        document,
        &[("src/main.rs", b"fn main() { LegacyFixture; }\n")],
    );
    assert!(!run.output.status.success());
    assert!(strings_at(&run.report, &["entrypointFindings"])
        .iter()
        .any(|finding| finding.contains("gpui-desktop-client")));
    assert!(strings_at(&run.report, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("forbidden legacy runtime")
            || blocker.contains("gpui-desktop-client")));
}

#[test]
fn compatibility_policy_reports_forbidden_runtime_switch() {
    let document = merge_contract(
        contract(
            vec![base_row(
                "legacy-app",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        json!({
            "compatibilityPolicy": {
                "permanentDualUi": false,
                "backwardCompatibilityMode": false,
                "forbiddenRuntimeSwitches": ["new_ui"],
                "scanPaths": ["src", "Cargo.toml"]
            }
        }),
    );
    let run = run_audit(
        document,
        &[("src/switch.rs", b"const MODE: &str = \"new_ui\";\n")],
    );
    assert!(!run.output.status.success());
    assert!(strings_at(&run.report, &["compatibilityFindings"]).contains(&"src/switch.rs"));
}

#[test]
fn packaging_handoff_reports_missing_required_files() {
    let document = merge_contract(
        contract(
            vec![base_row(
                "legacy-app",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        json!({
            "packagingHandoff": {
                "requiredBinaries": ["devmanager.exe", "devmanager-host.exe"],
                "atomicTwoBinaryIdentity": true,
                "packagerManifest": "Cargo.toml",
                "requiredManifestTokens": ["devmanager-host"],
                "requiredFiles": ["src/updater/handoff.rs"],
                "forbidInstallOrPublish": true
            }
        }),
    );
    let run = run_audit(document, &[("Cargo.toml", b"name = \"devmanager\"\n")]);
    assert!(!run.output.status.success());
    assert!(strings_at(&run.report, &["packagingFindings"])
        .iter()
        .any(|finding| finding.contains("devmanager-host")));
    assert!(strings_at(&run.report, &["packagingFindings"])
        .iter()
        .any(|finding| finding.contains("src/updater/handoff.rs")));
}

#[test]
fn profile_isolation_does_not_set_devmanager_profile_or_touch_installed_app() {
    let document = merge_contract(
        contract(
            vec![base_row(
                "legacy-app",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        json!({
            "profileIsolation": {
                "productionRootName": "com.userfirst.devmanager",
                "evidenceRoot": ".devmanager-next/evidence",
                "forbidSettingDevmanagerProfile": true,
                "remapAppData": true,
                "productionProfileOnlyInSignedRelease": true
            },
            "installedAppPolicy": {
                "touchInstalledApp": false,
                "hashProductionFiles": false,
                "openSessionJson": false,
                "installPublishDeleteUserData": false
            }
        }),
    );
    let run = run_audit(document, &[]);
    assert_eq!(run.report["isolation"]["setDevmanagerProfile"], false);
    assert_eq!(run.report["isolation"]["remappedAppData"], true);
    assert_eq!(
        run.report["isolation"]["inheritedDevmanagerProfileCleared"],
        true
    );
    assert_eq!(run.report["isolation"]["productionRootRead"], false);
    assert_eq!(
        run.report["installedApp"]["observedInstalledProcesses"],
        false
    );
    assert_eq!(run.report["installedApp"]["openSessionJson"], false);
    assert!(!run.human.contains("must-not-be-read"));
}

#[test]
fn entry_single_gpui_client_and_host_are_declared() {
    let contract = current_contract();
    assert_eq!(
        contract["productEntrypoints"]["desktopClient"]["path"],
        "src/main.rs"
    );
    assert_eq!(
        contract["productEntrypoints"]["desktopClient"]["role"],
        "gpui-client"
    );
    assert_eq!(
        contract["productEntrypoints"]["durableHost"]["path"],
        "src/bin/devmanager-host.rs"
    );
    assert_eq!(
        contract["productEntrypoints"]["durableHost"]["role"],
        "durable-host"
    );
    assert_eq!(
        contract["productEntrypoints"]["durableHost"]["lifecycle"],
        json!(["attach", "detach", "full-quit"])
    );
    assert_eq!(contract["compatibilityPolicy"]["permanentDualUi"], false);
    assert_eq!(
        contract["compatibilityPolicy"]["backwardCompatibilityMode"],
        false
    );
    assert_eq!(contract["packagingHandoff"]["forbidInstallOrPublish"], true);
    assert_eq!(
        contract["packagingHandoff"]["atomicTwoBinaryIdentity"],
        true
    );
    assert_eq!(
        contract["profileIsolation"]["productionProfileOnlyInSignedRelease"],
        true
    );
    assert_eq!(
        contract["publicationPolicy"]["requireExplicitManualApproval"],
        true
    );
    assert_eq!(contract["installedAppPolicy"]["touchInstalledApp"], false);
}

#[test]
fn entry_old_app_dispatch_and_devmanager_next_are_forbidden() {
    let contract = current_contract();
    let forbidden = contract["productEntrypoints"]["desktopClient"]["forbiddenDispatch"]
        .as_array()
        .expect("forbiddenDispatch");
    assert!(
        forbidden
            .iter()
            .any(|value| value == "devmanager::app::run"),
        "sole GPUI entry must forbid old app dispatch"
    );
    assert!(
        forbidden.iter().any(|value| value == "app::run"),
        "sole GPUI entry must forbid short old app dispatch"
    );
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/bin/devmanager-next.rs")
            .is_file(),
        "devmanager-next binary source must remain absent"
    );
    let next = current_row("legacy-next-entrypoint");
    assert_eq!(next["legacy"]["path"], "src/bin/devmanager-next.rs");
    assert_eq!(next["cutoverAction"], "delete");
    assert_eq!(
        next["status"], "DELETED",
        "an already-absent next binary must be DELETED, not a permanent HOLD"
    );
    let web_sessions = current_row("legacy-web-sessions");
    assert_eq!(web_sessions["legacy"]["path"], "web/src/sessions/");
    assert_eq!(web_sessions["cutoverAction"], "delete");
    assert_eq!(
        web_sessions["status"], "DELETED",
        "an already-absent web sessions tree must be DELETED, not a permanent HOLD"
    );
    let packaging_files = contract["packagingHandoff"]["requiredFiles"]
        .as_array()
        .expect("requiredFiles");
    assert!(packaging_files
        .iter()
        .any(|value| value == "src/updater/handoff.rs"));
    assert!(packaging_files
        .iter()
        .any(|value| value == "tests/update_contract.rs"));
    assert!(packaging_files
        .iter()
        .any(|value| value == "tests/package_contract.rs"));
}

#[test]
fn handoff_rows_are_distinct_from_deletion_rows() {
    let updater = current_row("handoff-updater-module");
    let update_contract = current_row("handoff-update-contract");
    assert_eq!(updater["cutoverAction"], "handoff");
    assert_eq!(update_contract["cutoverAction"], "handoff");
    assert!(updater.get("deletionSet").is_none());
    assert!(update_contract.get("deletionSet").is_none());
    assert_eq!(
        updater["replacementOwner"]["path"],
        "src/updater/handoff.rs"
    );
    assert_eq!(
        update_contract["replacementOwner"]["path"],
        "tests/update_contract.rs"
    );
    for row in current_rows() {
        if row["cutoverAction"] == "handoff" {
            continue;
        }
        assert!(
            row.get("deletionSet").is_some(),
            "delete rows must keep an exact deletionSet: {}",
            row["id"]
        );
    }
}

#[test]
fn session_json_is_path_only_in_contract() {
    let contract = current_contract();
    let protected = contract["referencePolicy"]["protectedFileBasenames"]
        .as_array()
        .expect("protected basenames");
    assert!(protected.iter().any(|value| value == "session.json"));
    assert_eq!(contract["installedAppPolicy"]["openSessionJson"], false);
    let session = current_row("legacy-session-persistence");
    assert_eq!(session["status"], "HOLD");
    assert!(session["legacy"]["symbols"]
        .as_array()
        .expect("session symbols")
        .iter()
        .any(|value| value == "session.json"));
}

#[test]
fn parity_current_ledger_is_hold() {
    let contract = current_contract();
    assert_eq!(contract["deletionPolicy"]["permanentHoldForbidden"], true);
    assert_eq!(contract["deletionPolicy"]["action"], "delete");
    assert_eq!(
        contract["deletionPolicy"]["deletedRequiresPathAndDeletionSetAbsent"],
        true
    );
    assert!(contract["prerequisiteNodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .all(|node| node["status"] == "HOLD"));
    let completed = expected_completed_deletion_ids();
    assert!(
        completed.iter().any(|id| id == "legacy-next-entrypoint"),
        "the next binary row must be a completed deletion"
    );
    assert!(
        completed.iter().any(|id| id == "legacy-web-sessions"),
        "the web sessions row must be a completed deletion"
    );
    assert!(
        completed.iter().any(|id| id == "legacy-codex-rollout"),
        "the Codex rollout tailer row must be a completed deletion"
    );
    assert!(
        completed.iter().any(|id| id == "legacy-tauri-archive"),
        "the archived Tauri tree row must be a completed deletion"
    );
    for row in current_rows() {
        if completed.iter().any(|id| row["id"] == *id) {
            assert_eq!(row["status"], "DELETED");
            continue;
        }
        assert_eq!(
            row["status"], "HOLD",
            "rows still waiting on owning-lane evidence must stay HOLD: {}",
            row["id"]
        );
    }
}

#[test]
fn old_rust_paths_remain_tracked_until_deleted() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/app/mod.rs",
        "src/services/process_manager.rs",
        "src/browser/pane.rs",
        "src/sidebar/mod.rs",
        "src/models/mod.rs",
        "src/persistence/mod.rs",
        "src/services/session_manager.rs",
    ] {
        assert!(
            root.join(relative).exists(),
            "legacy path should remain until a later approved deletion slice: {relative}"
        );
    }
    // The updater and package contracts are now part of the canonical Phase
    // 11 handoff. They must remain present while the audit ledger tracks the
    // old runtime paths above for a later, independently verified deletion.
    for relative in [
        "src/updater/handoff.rs",
        "src/host/update.rs",
        "tests/update_contract.rs",
        "tests/package_contract.rs",
    ] {
        assert!(
            root.join(relative).exists(),
            "Phase 11 handoff path should be present: {relative}"
        );
    }
}

#[test]
fn native_entry_cutover_source_contract_is_preserved() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let main = read_source("src/main.rs");
    assert!(main.contains("run_hook_relay_subcommand"));
    assert!(main.contains("run_codex_hook_relay_subcommand"));
    assert!(main.contains("run_native_shell"));
    assert!(!main.contains("devmanager::app::run()") && !main.contains("app::run()"));
    assert!(!main.contains("new_ui"));
    assert!(!main.contains("native_next"));
    assert!(!main.contains("use_old"));
    assert!(!main.contains("--legacy"));
    assert!(!root.join("src/bin/devmanager-next.rs").exists());
    let cargo = read_source("Cargo.toml");
    assert!(!cargo.contains("name = \"devmanager-next\""));
    assert!(cargo.contains("name = \"devmanager\""));
    assert!(cargo.contains("path = \"src/main.rs\""));
    assert!(cargo.contains("name = \"devmanager-host\""));
    assert!(cargo.contains("path = \"src/bin/devmanager-host.rs\""));
    assert!(cargo.contains("name = \"devmanager-provider-probe-fixture\""));
    assert!(cargo.contains("path = \"tests/fixtures/providers/probe_fixture.rs\""));

    let ui = read_source("src/ui/mod.rs");
    assert!(ui.contains("pub mod native_shell;"));
    assert!(ui.contains("pub mod terminal_adapter;"));
    assert!(root.join("src/ui/native_shell.rs").is_file());

    let host = read_source("src/bin/devmanager-host.rs");
    assert!(host.contains("parse_production_args"));
    assert!(host.contains("prepare_production_paths"));
    assert!(host.contains("PRODUCTION_HOST_PROFILE"));
    assert!(!host.contains("release host startup is deferred until Phase 11"));
    assert!(host.find("\"ctl\"").unwrap() < host.find("acquire_lock").unwrap());
    assert!(host.contains("dispatch_ctl_from_args"));

    let connection = read_source("src/client/connection.rs");
    assert!(connection.contains("map_named_pipe_open_error"));
    assert!(
        connection.contains("ERROR_FILE_NOT_FOUND")
            || connection.contains("raw_os_error() == Some(2)")
    );
    assert!(
        connection.contains("ERROR_PIPE_BUSY")
            || connection.contains("raw_os_error() == Some(231)")
    );

    let shell = read_source("src/ui/native_shell.rs");
    assert!(shell.contains("try_attach_existing_host"));
    assert!(shell.contains("DetachOnClientClose"));
    assert!(shell.contains("sanitize_spawned_host_environment"));
    assert!(shell.contains("Err(IpcError::Unavailable) => break"));
    assert!(shell.contains("Err(IpcError::Timeout)"));
    assert!(shell.contains("return Err(IpcError::Timeout)"));
    assert!(!shell.contains("\"devmanager-next/"));
    let production_args = shell
        .split("NativeHostLaunchMode::Production =>")
        .nth(1)
        .unwrap_or_default();
    assert!(production_args.contains("--foreground"));
    assert!(!production_args
        .lines()
        .take(8)
        .any(|line| line.contains("--parent-pid")));

    let main_body = main.split("fn main()").nth(1).expect("main function body");
    let claude = main_body.find("run_hook_relay_subcommand").unwrap();
    let codex = main_body.find("run_codex_hook_relay_subcommand").unwrap();
    let preview = main_body.find("--ui-preview").unwrap();
    let product = main_body.find("run_product_shell").unwrap();
    assert!(claude < codex && codex < preview.min(product));

    let preview_source = read_source("src/ui/preview.rs");
    assert!(preview_source.contains("usage: devmanager --ui-preview"));
    assert!(!preview_source.contains("devmanager-next --ui-preview"));
    assert!(!preview_source.contains("gpui::actions!(devmanager_next"));
    let capture = read_source("scripts/native-next/Capture-UiPreviews.ps1");
    assert!(capture.contains("$artifactName = 'devmanager'"));
    assert!(capture.contains("$artifactBinaryName = 'devmanager.exe'"));
    assert!(capture.contains("'--bin', 'devmanager'"));
    assert!(!capture.contains("$artifactName = 'devmanager-next'"));
    assert!(!capture.contains("'--bin', 'devmanager-next'"));
}

#[test]
fn entry_deletion_semantics_require_deleted_or_exact_deferred_paths() {
    let contract = current_contract();
    let deferred = contract["deferredDeletionPaths"]
        .as_array()
        .expect("deferredDeletionPaths")
        .iter()
        .map(|value| value.as_str().expect("deferred path").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(deferred, expected_deferred_deletion_paths());
    assert!(
        !deferred.is_empty(),
        "active lanes still have deferred deletions"
    );
    assert!(
        !deferred
            .iter()
            .any(|path| path == "src/bin/devmanager-next.rs" || path == "web/src/sessions/"),
        "completed absences must not be listed as deferred"
    );

    for row in current_delete_rows() {
        let id = row["id"].as_str().expect("row id");
        let legacy = row["legacy"]["path"].as_str().expect("legacy path");
        let deletion_set = row["deletionSet"]
            .as_array()
            .expect("deletionSet")
            .iter()
            .map(|value| value.as_str().expect("deletion path"))
            .collect::<Vec<_>>();
        assert!(
            deletion_set.contains(&legacy),
            "deletionSet must include the legacy owner: {id}"
        );
        let action = row
            .get("cutoverAction")
            .and_then(Value::as_str)
            .unwrap_or("delete");
        assert_eq!(action, "delete", "delete policy must stay delete: {id}");

        let all_absent = !current_legacy_path_present(legacy)
            && deletion_set
                .iter()
                .all(|path| !current_legacy_path_present(path));
        if all_absent {
            assert_eq!(
                row["status"], "DELETED",
                "absent path and deletionSet must be DELETED: {id}"
            );
            assert!(
                !deferred.iter().any(|path| path == legacy),
                "DELETED legacy path must not appear in deferredDeletionPaths: {id}"
            );
        } else {
            assert_eq!(
                row["status"], "HOLD",
                "a still-present legacy path cannot claim DELETED: {id}"
            );
            assert!(
                deferred.iter().any(|path| path == legacy),
                "present legacy path must be an exact deferredDeletionPaths entry: {id}"
            );
            assert!(
                current_legacy_path_present(legacy)
                    || deletion_set
                        .iter()
                        .any(|path| current_legacy_path_present(path)),
                "HOLD delete rows must still have a present deletion path: {id}"
            );
        }
    }

    let completed = expected_completed_deletion_ids();
    assert_eq!(
        completed,
        vec![
            "legacy-next-entrypoint".to_owned(),
            "legacy-codex-rollout".to_owned(),
            "legacy-web-sessions".to_owned(),
            "legacy-tauri-archive".to_owned()
        ]
    );
}

#[test]
fn host_serve_request_is_an_integration_test_compatibility_seam() {
    let seam = &current_contract()["hostCompatibility"]["serveRequest"];
    assert_eq!(seam["path"], "src/host/ipc.rs");
    assert_eq!(seam["symbol"], "HostConnection::serve_request");
    assert_eq!(seam["kind"], "integration-test-seam");
    assert_eq!(seam["cfgTestGated"], false);
    assert_eq!(seam["productionSymbol"], "HostConnection::serve_duplex");
    assert_eq!(seam["productionCaller"], "src/bin/devmanager-host.rs");

    let ipc = read_source("src/host/ipc.rs");
    assert!(ipc.contains("pub async fn serve_request("));
    assert!(ipc.contains("pub async fn serve_request_on_executor("));
    assert!(ipc.contains("pub async fn serve_duplex("));
    assert!(
        ipc.contains("Integration-test compatibility path used by `tests/ipc_protocol.rs`")
            || ipc.contains("Exclusive compatibility path used by ipc_protocol tests")
    );
    assert!(
        !ipc.contains("#[cfg(test)]\n    pub async fn serve_request(")
            && !ipc.contains("#[cfg(test)]\npub async fn serve_request("),
        "cfg(test) would hide serve_request from tests/ipc_protocol.rs"
    );

    let host = read_source("src/bin/devmanager-host.rs");
    assert!(host.contains(".serve_duplex("));
    assert!(!host.contains(".serve_request("));

    let shell = read_source("src/ui/native_shell.rs");
    assert!(!shell.contains("serve_request"));

    let ipc_tests = read_source("tests/ipc_protocol.rs");
    assert!(
        ipc_tests.contains(".serve_request("),
        "keep the existing ipc_protocol compatibility caller; do not silently drop the seam"
    );
}

#[test]
fn phase11_codex_rollout_is_removed_as_identity_source() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("src/ai/codex_rollout.rs").exists(),
        "codex_rollout must not remain an identity or transcript source"
    );

    let ai = read_source("src/ai/mod.rs");
    assert!(
        !ai.contains("codex_rollout"),
        "ai module must not export the deleted rollout tailer"
    );
    assert!(ai.contains("pub mod claude_hooks;"));
    assert!(ai.contains("pub mod codex_cli;"));
    assert!(ai.contains("pub mod codex_hooks;"));

    let process_manager = read_source("src/services/process_manager.rs");
    assert!(!process_manager.contains("crate::ai::codex_rollout"));
    assert!(!process_manager.contains("CodexRolloutTailer"));
    assert!(!process_manager.contains("CodexRolloutReducer"));
    assert!(process_manager.contains("bind_runtime_provider_session_id"));
    assert!(process_manager.contains("CodexRegistryEvent::SessionStarted"));

    let session_started = process_manager
        .split("fn handle_codex_session_started")
        .nth(1)
        .expect("handle_codex_session_started")
        .split("fn bind_runtime_provider_session_id")
        .next()
        .expect("session-start handler body");
    assert!(
        session_started.contains("bind_runtime_provider_session_id("),
        "SessionStart must keep hook-correlated provider identity"
    );
    assert!(
        !session_started.contains("transcript_path"),
        "SessionStart must not tail or infer identity from a rollout transcript path"
    );
    assert!(
        !session_started.contains("cwd"),
        "SessionStart must not infer identity from cwd"
    );

    let row = current_row("legacy-codex-rollout");
    assert_eq!(row["cutoverAction"], "delete");
    assert_eq!(row["status"], "DELETED");
    assert_eq!(row["legacy"]["path"], "src/ai/codex_rollout.rs");
}

#[test]
fn phase11_tauri_archive_is_absent() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("zz-archive/tauri-react-v0.1.11").exists(),
        "archived Tauri desktop tree must be absent"
    );
    assert!(
        !root.join("zz-archive").exists(),
        "zz-archive must be absent once the dead archive is deleted"
    );

    let row = current_row("legacy-tauri-archive");
    assert_eq!(row["cutoverAction"], "delete");
    assert_eq!(row["status"], "DELETED");
    assert_eq!(row["legacy"]["path"], "zz-archive/tauri-react-v0.1.11/");

    let contract = current_contract();
    let deferred = contract["deferredDeletionPaths"]
        .as_array()
        .expect("deferredDeletionPaths");
    assert!(!deferred
        .iter()
        .any(|value| value == "zz-archive/tauri-react-v0.1.11/"));
    assert!(!deferred
        .iter()
        .any(|value| value == "src/ai/codex_rollout.rs"));

    let scanner = read_source("src/services/scanner_service.rs");
    assert!(
        scanner.contains("\"zz-archive\""),
        "scanner skip-name must remain even after the archive directory is gone"
    );
}

#[test]
fn phase11_legacy_app_sidebar_and_session_manager_remain_held() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lib = read_source("src/lib.rs");
    assert!(
        lib.contains("pub mod app;"),
        "app remains exported while unowned tests still include_str its source"
    );
    assert!(
        lib.contains("pub mod sidebar;"),
        "sidebar remains exported while the held app runtime imports it"
    );
    assert!(root.join("src/app/mod.rs").is_file());
    assert!(root.join("src/app/chrome.rs").is_file());
    assert!(root.join("src/app/process_monitor.rs").is_file());
    assert!(root.join("src/sidebar/mod.rs").is_file());
    assert!(root.join("src/services/session_manager.rs").is_file());

    let app = current_row("legacy-app-runtime");
    assert_eq!(app["status"], "HOLD");
    let app_hold = app["approvalRequirement"].as_str().expect("app hold");
    assert!(
        app_hold.contains("tests/browser_pane.rs")
            && app_hold.contains("include_str")
            && app_hold.contains("src/app/mod.rs"),
        "app HOLD must name the exact remaining source-inspection dependents: {app_hold}"
    );

    let sidebar = current_row("legacy-sidebar");
    assert_eq!(sidebar["status"], "HOLD");
    let sidebar_hold = sidebar["approvalRequirement"]
        .as_str()
        .expect("sidebar hold");
    assert!(
        sidebar_hold.contains("src/app/mod.rs")
            && sidebar_hold.contains("use crate::sidebar")
            && sidebar_hold.contains("sidebarCollapsed"),
        "sidebar HOLD must name the app import and the persistence data-contract field: {sidebar_hold}"
    );

    let session = current_row("legacy-session-manager");
    assert_eq!(session["status"], "HOLD");
    let session_hold = session["approvalRequirement"]
        .as_str()
        .expect("session hold");
    assert!(
        session_hold.contains("src/services/mod.rs")
            && session_hold.contains("ConfigImportMode")
            && session_hold.contains("SessionManager")
            && session_hold.contains("tests/config_persistence.rs")
            && session_hold.contains("apply_import_mode"),
        "session_manager HOLD must name the remaining export and test helpers: {session_hold}"
    );

    let services = read_source("src/services/mod.rs");
    assert!(services.contains("mod session_manager;"));
    assert!(services.contains("pub use session_manager::{ConfigImportMode, SessionManager};"));
    let config = read_source("src/config/mod.rs");
    assert!(
        !config.contains("apply_import_mode") && !config.contains("ConfigImportMode"),
        "config facade does not yet own SessionManager import-merge helpers"
    );
}
