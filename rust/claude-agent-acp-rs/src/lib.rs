#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod codec;
pub mod control;
pub mod dispatch;
pub mod map;
pub mod process;
pub mod session;
pub mod turn;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
