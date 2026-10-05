//! Fixture for the extraction guard only: the forms widget.rs leaves out —
//! constants, a macro, a type alias, crate visibility, items in a macro body.

pub const LIMIT: u32 = 10;
static COUNTER: u32 = 0;

pub type Size = u32;

macro_rules! cfg_gadget {
    ($($item:item)*) => { $($item)* };
}

cfg_gadget! {
    pub struct Gadget {
        pub(crate) size: Size,
    }

    impl Gadget {
        pub(crate) const DEFAULT: u32 = 1;

        pub(crate) fn spin(&self) -> u32 {
            self.size + LIMIT + COUNTER
        }
    }
}

pub(crate) fn shared() {}
