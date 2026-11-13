#![forbid(unsafe_code)]
#[path = "support/auth_fixture.rs"]
mod fixture;
macro_rules! case {
    ($name:ident,$id:literal) => {
        #[test]
        fn $name() {
            fixture::run_case($id).expect($id);
        }
    };
}
case!(jwt, "jwt");
case!(lifecycle, "lifecycle");
case!(permission, "permission");
case!(refresh_crash, "refresh-crash");
case!(pending_cleanup, "pending-cleanup");
case!(dispatch_logout, "dispatch-logout");
case!(logout_login, "logout-login");
case!(logout_refresh, "logout-refresh");
case!(logout_crash, "logout-crash");
case!(logout_marker, "logout-marker");
case!(refresh_scope, "refresh-scope");
case!(uncertain_login, "uncertain-login");
case!(held_dispatch, "held-dispatch");
case!(committed_login_crash, "committed-login-crash");
case!(execution_logout, "execution-logout");
#[test]
fn process_refresh() {
    if let Some(root) = std::env::var_os("ANSA_M3_FIXTURE_ROOT") {
        let action = std::env::var("ANSA_M3_FIXTURE_ACTION").expect("fixture action");
        let now = std::env::var("ANSA_M3_FIXTURE_NOW")
            .expect("fixture clock")
            .parse()
            .expect("fixture clock integer");
        fixture::helper(std::path::Path::new(&root), &action, now).expect("fixture child");
    } else {
        fixture::run_case("process-refresh").expect("process-refresh");
    }
}
