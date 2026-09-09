//! Fallback for platforms with neither process groups nor job objects: kill
//! the direct child only, and accept that a grandchild may survive.

use std::process::{Child, Command};

pub fn configure(_cmd: &mut Command) {}

pub fn kill_tree(child: &mut Child) {
    let _ = child.kill();
}
