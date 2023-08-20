Rust crates to Debian packages using vendoring
==============================================

`debcargo-vendor` is a fork of [`debcargo`](https://salsa.debian.org/rust-team/debcargo) that
uses Cargo vendoring to create Debian packaging for Rust crates. This simplifies distribution
of Rust crates for internal use, however it is not well-suited for creating packages suitable for
inclusion in the Debian project as it ignores the [`debcargo-conf`](https://salsa.debian.org/rust-team/debcargo-conf)
and general Debian project packaging guidelines. 
