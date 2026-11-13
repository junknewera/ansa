mod support {
    pub mod tools_fixture;
}
use support::tools_fixture as f;
#[test]
fn config() {
    f::configuration();
}
#[test]
fn instruction_links() {
    f::instruction_links();
}
#[test]
fn files() {
    f::files();
}
#[test]
fn process_basic() {
    f::basic();
}
#[test]
fn raw_filenames() {
    f::raw_names();
}
#[test]
fn future_auth_root() {
    f::future_auth_root();
}
#[test]
fn process_cancel() {
    f::cancellation(false);
}
#[test]
fn process_timeout() {
    f::cancellation(true);
}
#[test]
fn process_detached() {
    f::detached();
}
#[test]
fn process_output() {
    f::process_output();
}
#[test]
fn process_privileges() {
    f::privileges();
}
#[test]
fn process_evidence() {
    f::evidence();
}
#[test]
fn runtime_nonzero_continues() {
    f::runtime_loop(false);
}
#[test]
fn runtime_process_cancel() {
    f::runtime_loop(true);
}
