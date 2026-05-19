//! Negative fixture: `KeyedPermit`'s inner fields are private, so user
//! code cannot write a struct literal to forge a permit. The only way
//! to obtain one is `KeyedLimit::try_admit(...)`, which goes through
//! the gate's bookkeeping.

use std::marker::PhantomData;

use tina_runtime::KeyedPermit;

fn main() {
    let _forged: KeyedPermit<u32> = KeyedPermit {
        inner: None,
        _key: PhantomData,
    };
}
