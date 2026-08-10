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
const FIXTURE_AUTH_TOKEN: &str = "phase-11.1a-generated-fixture-v1";

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

fn fixture_repo(document: Value, extra_files: &[(&str, &[u8])]) -> FixtureRepo {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let root = temp.path().to_path_buf();
    fs::create_dir_all(root.join("docs")).expect("fixture docs");
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::create_dir_all(root.join(".devmanager-next")).expect("fixture audit auth directory");
    fs::write(
        root.join(".devmanager-next/audit-fixture.auth"),
        format!("{FIXTURE_AUTH_TOKEN}\n"),
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
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", FIXTURE_AUTH_TOKEN)
        .output()
        .expect("spawn cutover audit")
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
    let original_path = std::env::var_os("PATH").expect("PATH");
    let mut path_entries = vec![shim_root.to_path_buf()];
    path_entries.extend(std::env::split_paths(&original_path));
    let isolated_path = std::env::join_paths(path_entries).expect("isolated PATH");
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
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", FIXTURE_AUTH_TOKEN)
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
    assert!(strings_at(legacy, &["references", "path"]).contains(&"src/reference.rs"));
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
    assert!(row(&run.report, "trailing-slash")["legacy"]["path"].is_null());
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
    assert!(first_report["rows"]
        .as_array()
        .expect("current rows")
        .iter()
        .all(|row| row["status"] == "HOLD"));
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
    let original_path = std::env::var_os("PATH").expect("PATH");
    let mut path_entries = vec![shim_root.clone()];
    path_entries.extend(std::env::split_paths(&original_path));
    let isolated_path = std::env::join_paths(path_entries).expect("isolated PATH");
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
        .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", FIXTURE_AUTH_TOKEN)
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
    assert!(strings_at(&report, &["blockers"])
        .iter()
        .any(|blocker| *blocker == "audit[path_reparse_rejected]"));
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
        let original_path = std::env::var_os("PATH").expect("PATH");
        let mut path_entries = vec![shim_root.clone()];
        path_entries.extend(std::env::split_paths(&original_path));
        let isolated_path = std::env::join_paths(path_entries).expect("isolated PATH");
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
            .env("DEVMANAGER_CUTOVER_FIXTURE_AUTH", FIXTURE_AUTH_TOKEN)
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
    let original_path = std::env::var_os("PATH").expect("PATH");
    let mut path_entries = vec![shim_root];
    path_entries.extend(std::env::split_paths(&original_path));
    let isolated_path = std::env::join_paths(path_entries).expect("isolated PATH");
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
