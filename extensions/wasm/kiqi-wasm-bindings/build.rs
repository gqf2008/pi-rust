//! Rebuild the guest SDK (and therefore every guest that binds it) when the
//! shared WIT contract changes. `wit_bindgen::generate!` embeds the WIT at
//! compile time, but cargo cannot see that file as an input on its own — a
//! cached SDK otherwise keeps the old bindings after a WIT edit.

fn main() {
    println!("cargo:rerun-if-changed=../../kiqi-extension.wit");
}
