//! Negative fixture: a `KeyedPermit` is move-only, so the second
//! `release(...)` after the first fails with "value used after move".
//! This is the structural guarantee that a single admitted charge cannot
//! be released twice from user code.

use tina_runtime::KeyedLimit;

fn main() {
    let mut limit = KeyedLimit::<&'static str>::new("conc.compile_fail", 2, 2);
    let permit = limit.try_admit(&"alpha").into_admitted().unwrap();
    let _ = limit.release(permit);
    let _ = limit.release(permit);
}
