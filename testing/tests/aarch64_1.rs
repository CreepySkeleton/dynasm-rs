#![feature(proc_macro_hygiene)]
#![allow(unused_imports)]

#[macro_use]
extern crate dynasmrt;
extern crate dynasm;

use dynasm::dynasm;
use dynasmrt::DynasmApi;

include!("gen_aarch64/aarch64_tests_1.rs.gen");
