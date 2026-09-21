#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod codec;
pub mod control;
pub mod dispatch;
pub mod map;
pub mod permission;
pub mod process;
pub mod session;
pub mod tools;
pub mod turn;

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
