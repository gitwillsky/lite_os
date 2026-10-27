//! 补齐 Clippy 对未按 host/test target 编译的裸机源码的检查覆盖。
use std::{fs, path::Path};
use syn::{
    Attribute, Item, Meta, punctuated::Punctuated, spanned::Spanned, token::Comma, visit::Visit,
};

use super::repository_source::rust_files;

#[derive(Default)]
struct ConstantAssertions {
    violations: Vec<usize>,
}
impl<'ast> Visit<'ast> for ConstantAssertions {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function
            .attrs
            .iter()
            .any(|attribute| attribute.path().is_ident("test"))
        {
            syn::visit::visit_block(self, &function.block);
        }
    }

    fn visit_macro(&mut self, assertion: &'ast syn::Macro) {
        let Some(name) = assertion
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        else {
            return;
        };
        if !matches!(name.as_str(), "assert" | "assert_eq" | "assert_ne") {
            return;
        }
        let Ok(arguments) =
            assertion.parse_body_with(Punctuated::<syn::Expr, Comma>::parse_terminated)
        else {
            return;
        };
        let count = if name == "assert" { 1 } else { 2 };
        // 只拒绝直接字面量断言；ABI 常量、size_of 和生产调用都保留，由 Review 判断语义价值。
        if arguments.len() >= count
            && arguments
                .iter()
                .take(count)
                .all(|expr| matches!(expr, syn::Expr::Lit(_)))
        {
            self.violations.push(assertion.span().start().line);
        }
    }
}

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let Ok(conditions) = list.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                conditions.iter().any(requires_test)
            } else {
                !conditions.is_empty() && conditions.iter().all(requires_test)
            }
        }
        _ => false,
    }
}

fn test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<Meta>()
                .is_ok_and(|meta| requires_test(&meta))
    })
}

fn attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

#[derive(Default)]
struct Layout {
    violations: Vec<usize>,
}
impl Layout {
    fn items(&mut self, items: &[Item]) {
        let mut tests_started = false;
        for item in items {
            let only_test = test_only(attributes(item));
            if tests_started && !only_test {
                self.violations.push(item.span().start().line);
            }
            if matches!(item, Item::Mod(_)) && only_test {
                tests_started = true;
            }
        }
    }
}
impl<'ast> Visit<'ast> for Layout {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.items(&file.items);
        syn::visit::visit_file(self, file);
    }
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        // 测试模块内部全部属于测试上下文；不将测试 helper 当成产品实现。
        if test_only(&module.attrs) {
            return;
        }
        if let Some((_, items)) = &module.content {
            self.items(items);
        }
        syn::visit::visit_item_mod(self, module);
    }
}

/// 对全部第一方 Rust 源码执行与 items_after_test_module 一致的模块边界约束。
///
/// # Parameters
/// - `root`: 仓库根目录。
/// - `errors`: 接收不可读取/解析或测试模块之后出现生产条目的错误。
///
/// # Returns
/// 无返回值；读取源码，不修改或执行它。
pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if let Err(error) = rust_files(root, &mut files) {
        errors.push(error);
        return;
    }
    files.sort();
    for path in files {
        let source = fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| syn::parse_file(&text).map_err(|error| error.to_string()));
        let file = match source {
            Ok(file) => file,
            Err(error) => {
                errors.push(format!(
                    "{}: Rust source parse failed: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let mut layout = Layout::default();
        layout.visit_file(&file);
        let mut assertions = ConstantAssertions::default();
        assertions.visit_file(&file);
        for line in assertions.violations {
            errors.push(format!("{}:{line}: test assertion compares only literals; exercise a production contract or remove the redundant test", path.strip_prefix(root).expect("source below root").display()));
        }
        for line in layout.violations {
            errors.push(format!("{}:{line}: production item follows a #[cfg(test)] module; move the test modules to the end (clippy::items_after_test_module)", path.strip_prefix(root).expect("source below root").display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_assertion_gate_rejects_literals_without_rejecting_abi_or_runtime_checks() {
        let mut assertions = ConstantAssertions::default();
        assertions.visit_file(&syn::parse_file(r#"
            const TEXT: &str = "assert!(true)";
            #[cfg(test)] mod tests {
                #[test] fn redundant() { assert!(true); assert_eq!(1, 1); assert_ne!(1, 2); }
                #[test] fn useful() { assert_eq!(SYSCALL_NUMBER, 117); assert_eq!(size_of::<Layout>(), 64); assert_eq!(production(), 1); }
            }
        "#).unwrap());
        assert_eq!(assertions.violations.len(), 3);
    }

    fn violations(source: &str) -> Vec<usize> {
        let mut layout = Layout::default();
        layout.visit_file(&syn::parse_file(source).unwrap());
        layout.violations
    }

    #[test]
    fn rejects_real_module_items_but_ignores_strings_and_comments() {
        assert_eq!(
            violations("#[cfg(test)] mod tests {}\nconst ABI: usize = 117;"),
            [2]
        );
        assert!(violations("const TEXT: &str = r#\"#[cfg(test)] mod tests {}\"#;\n// mod tests {}\nfn production() {}\n#[cfg(test)] mod tests {}").is_empty());
    }
    #[test]
    fn checks_nested_and_external_modules_without_reclassifying_test_helpers() {
        assert_eq!(
            violations("mod owner { #[cfg(test)] mod tests;\nfn production() {} }"),
            [2]
        );
        assert!(violations("fn production() {} #[cfg(test)] mod tests { #[test] fn test() {} fn helper() {} } #[cfg(test)] mod more_tests;").is_empty());
    }
    #[test]
    fn feature_union_remains_production_and_test_conjunction_remains_tests() {
        assert_eq!(
            violations(
                "#[cfg(all(test, target_arch = \"aarch64\"))] mod tests {}\n#[cfg(any(test, feature = \"runtime\"))] fn production() {}"
            ),
            [2]
        );
        assert!(
            violations(
                "#[cfg(any(test, feature = \"runtime\"))] mod diagnostics {} fn production() {}"
            )
            .is_empty()
        );
    }
}
