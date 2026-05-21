//! Unit tests for `provbench_phase1::rules::r3_python_resolver`.
//!
//! Per SPEC §5.2 v1.4 amendment and design doc
//! `docs/superpowers/specs/2026-05-21-r3-retuning-v1.4-design.md`, the
//! resolver returns true iff `leaf` matches a Python declaration node
//! (function/class/decorated/module-or-class assignment).
//!
//! Conservative on parse failure / empty / non-UTF8: returns true.

use provbench_phase1::rules::r3_python_resolver::resolves_in_python;

#[test]
fn function_definition_found() {
    let src = b"def foo():\n    pass\n";
    assert!(resolves_in_python(src, "foo"));
}

#[test]
fn class_definition_found() {
    let src = b"class Foo:\n    pass\n";
    assert!(resolves_in_python(src, "Foo"));
}

#[test]
fn method_in_class_found() {
    let src = b"class A:\n    def m(self):\n        pass\n";
    assert!(resolves_in_python(src, "m"));
}

#[test]
fn module_level_assignment_found() {
    let src = b"FOO = 1\n";
    assert!(resolves_in_python(src, "FOO"));
}

#[test]
fn class_attribute_assignment_found() {
    let src = b"class A:\n    x = 1\n";
    assert!(resolves_in_python(src, "x"));
}

#[test]
fn decorated_function_found() {
    let src = b"@cache\ndef foo():\n    pass\n";
    assert!(resolves_in_python(src, "foo"));
}

#[test]
fn decorated_class_found() {
    let src = b"@dataclass\nclass A:\n    pass\n";
    assert!(resolves_in_python(src, "A"));
}

#[test]
fn nested_decorators_found() {
    let src = b"@a\n@b\ndef foo():\n    pass\n";
    assert!(resolves_in_python(src, "foo"));
}

#[test]
fn symbol_absent_returns_false() {
    let src = b"def other():\n    pass\n";
    assert!(!resolves_in_python(src, "foo"));
}

#[test]
fn local_variable_inside_function_excluded() {
    // `x` is a function-local; not a declaration we count.
    let src = b"def f():\n    x = 1\n";
    assert!(!resolves_in_python(src, "x"));
}

#[test]
fn function_parameter_excluded() {
    // `x` is a parameter; not a declaration we count.
    let src = b"def f(x):\n    pass\n";
    assert!(!resolves_in_python(src, "x"));
}

#[test]
fn identifier_use_excluded() {
    // `foo` is referenced but not declared.
    let src = b"print(foo)\n";
    assert!(!resolves_in_python(src, "foo"));
}

#[test]
fn syntax_error_returns_true_conservative() {
    // Malformed Python — conservative fallback returns Valid.
    let src = b"def f(\n  pass\n";
    assert!(resolves_in_python(src, "f"));
    assert!(resolves_in_python(src, "anything"));
}

#[test]
fn empty_input_returns_true_conservative() {
    let src = b"";
    assert!(resolves_in_python(src, "foo"));
}

#[test]
fn non_utf8_returns_true_conservative() {
    // Invalid UTF-8 prefix; conservative fallback returns Valid.
    let src: &[u8] = &[0xff, 0xfe, b'd', b'e', b'f', b' ', b'f', b':'];
    assert!(resolves_in_python(src, "f"));
}

#[test]
fn dunder_name_found() {
    // Common short names (e.g., __init__) are also AST-declarations.
    let src = b"class A:\n    def __init__(self):\n        pass\n";
    assert!(resolves_in_python(src, "__init__"));
}

#[test]
fn same_leaf_in_multiple_classes_resolves() {
    // If two classes define `m`, the leaf resolves — phase1 can't
    // tell which one is "ours". Conservative-Valid by design.
    let src = b"class A:\n    def m(self):\n        pass\nclass B:\n    def m(self):\n        pass\n";
    assert!(resolves_in_python(src, "m"));
}
