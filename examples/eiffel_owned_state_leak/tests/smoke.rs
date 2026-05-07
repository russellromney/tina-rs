use eiffel_owned_state_leak::{
    DOCUMENTED_COMPILE_FAILS, INTENTIONAL_ESCAPE_WRITES, Report, tina_impl,
};

#[test]
fn tina_smoke() {
    let r: Report = tina_impl::run().expect("tina side ran");
    assert_eq!(
        r,
        Report {
            documented_compile_fails: DOCUMENTED_COMPILE_FAILS,
            intentional_escape_writes: INTENTIONAL_ESCAPE_WRITES,
            exit_clean: true,
        }
    );
}
