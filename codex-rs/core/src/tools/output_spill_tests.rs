use super::*;
use pretty_assertions::assert_eq;

#[test]
fn spills_content_under_thread_scoped_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::default();
    let spill = ExecOutputSpill {
        codex_home: dir.path(),
        thread_id,
        call_id: "call-1",
    };

    let path = spill_exec_output(&spill, "full output body").expect("spill should succeed");

    assert_eq!(
        path,
        dir.path()
            .join("link")
            .join("tool-output")
            .join(thread_id.to_string())
            .join("call-1.txt")
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("spill file should be readable"),
        "full output body"
    );
}

#[test]
fn sanitizes_hostile_call_ids() {
    assert_eq!(sanitize_call_id("../../etc/passwd"), ".._.._etc_passwd");
    assert_eq!(sanitize_call_id("call/1:2"), "call_1_2");
    assert_eq!(sanitize_call_id("..."), "call");
    assert_eq!(sanitize_call_id("call-1_a.B"), "call-1_a.B");
}
