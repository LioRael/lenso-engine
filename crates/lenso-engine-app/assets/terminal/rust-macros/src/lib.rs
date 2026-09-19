use proc_macro::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn, LitStr, Pat, Type, parse_macro_input};

/// Lower one ordinary function in a cli.rs entry to the terminal Provider contract.
#[proc_macro_attribute]
pub fn command(args: TokenStream, input: TokenStream) -> TokenStream {
    let function = parse_macro_input!(input as ItemFn);
    expand(args.into(), function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(
    args: proc_macro2::TokenStream,
    mut function: ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut name = function
        .sig
        .ident
        .to_string()
        .trim_start_matches("r#")
        .replace('_', "-");
    let mut named = false;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            if named {
                return Err(meta.error("duplicate command name"));
            }
            named = true;
            name = meta.value()?.parse::<LitStr>()?.value();
            Ok(())
        } else {
            Err(meta.error("expected name = \"command-name\""))
        }
    });
    syn::parse::Parser::parse2(parser, args)?;
    if name.split(' ').any(|part| !valid_name(part)) {
        return Err(syn::Error::new_spanned(
            &function.sig.ident,
            "invalid command name",
        ));
    }
    if function.sig.ident == "command"
        || !function.sig.generics.params.is_empty()
        || function.sig.unsafety.is_some()
        || function.sig.abi.is_some()
    {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "command functions must be safe, non-generic Rust functions named other than command",
        ));
    }
    let description = docs(&function.attrs);
    let mut definitions = vec![];
    let mut conversions = vec![];
    let mut arguments = vec![];
    let mut names = std::collections::BTreeSet::new();
    let mut has_context = false;
    for argument in &mut function.sig.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "command functions cannot have self",
            ));
        };
        let Pat::Ident(binding) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "command arguments must have identifier patterns",
            ));
        };
        if binding.by_ref.is_some() || binding.subpat.is_some() {
            return Err(syn::Error::new_spanned(
                binding,
                "unsupported command argument pattern",
            ));
        }
        let ident = binding.ident.clone();
        if ident.to_string().starts_with("__lenso_") {
            return Err(syn::Error::new_spanned(
                &ident,
                "the __lenso_ prefix is reserved for generated command bindings",
            ));
        }
        let ty = argument.ty.as_ref();
        let is_context = argument.attrs.iter().any(|a| a.path().is_ident("context"));
        if is_context {
            for attr in argument
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("context"))
            {
                if !matches!(attr.meta, syn::Meta::Path(_)) {
                    return Err(syn::Error::new_spanned(
                        attr,
                        "use #[context] without options",
                    ));
                }
            }
            if has_context || argument.attrs.iter().any(|a| a.path().is_ident("arg")) {
                return Err(syn::Error::new_spanned(
                    argument,
                    "use one context parameter without arg options",
                ));
            }
            has_context = true;
            conversions.push(quote! { let #ident: #ty = __lenso_context.clone(); });
        } else {
            let mut long = ident.to_string().trim_start_matches("r#").replace('_', "-");
            let mut default = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            for attr in argument.attrs.iter().filter(|a| a.path().is_ident("arg")) {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("long") {
                        if !seen.insert("long") {
                            return Err(meta.error("duplicate long option"));
                        }
                        if meta.input.peek(syn::Token![=]) {
                            long = meta.value()?.parse::<LitStr>()?.value();
                        }
                        Ok(())
                    } else if meta.path.is_ident("default") {
                        if !seen.insert("default") {
                            return Err(meta.error("duplicate default"));
                        }
                        default = Some(meta.value()?.parse::<LitStr>()?.value());
                        Ok(())
                    } else {
                        Err(meta.error("expected long or default"))
                    }
                })?;
            }
            if !valid_name(&long) || long == "help" || !names.insert(long.clone()) {
                return Err(syn::Error::new_spanned(
                    argument,
                    "invalid, reserved, or duplicate long option",
                ));
            }
            let optional = option_inner(ty);
            let flag = matches!(ty, Type::Path(p) if p.path.is_ident("bool"));
            let required = !flag && optional.is_none() && default.is_none();
            let fallback = match default {
                Some(value) => quote!(Some(#value)),
                None => quote!(None),
            };
            let help = docs(&argument.attrs);
            definitions.push(quote! { .argument(#long, #fallback, #required, #flag, #help) });
            if let Some(inner) = optional {
                conversions.push(quote! { let #ident: #ty = __lenso_args.get(#long).map(|value| ::lenso_cli_support::parse_argument::<#inner>(value)).transpose()?; });
            } else {
                conversions.push(quote! { let #ident: #ty = ::lenso_cli_support::parse_argument(__lenso_args.get(#long).ok_or(::lenso_cli_support::CommandError::InvalidArguments)?)?; });
            }
        }
        arguments.push(quote!(#ident));
        argument
            .attrs
            .retain(|a| !a.path().is_ident("arg") && !a.path().is_ident("context"));
    }
    let ident = &function.sig.ident;
    let call = if function.sig.asyncness.is_some() {
        quote!(#ident(#(#arguments),*).await)
    } else {
        quote!(#ident(#(#arguments),*))
    };
    Ok(quote! {
        #function
        pub fn command() -> ::lenso_cli_support::Command {
            ::lenso_cli_support::Command::new(#name, #description)
                #(#definitions)*
                .run_async(|__lenso_args, __lenso_context| Box::pin(async move {
                    #(#conversions)*
                    ::lenso_cli_support::CommandReturn::finish(#call, &__lenso_context)
                }))
        }
    })
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
        Some(inner)
    } else {
        None
    }
}
fn docs(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter_map(|attr| {
            if !attr.path().is_ident("doc") {
                return None;
            }
            let syn::Meta::NameValue(value) = &attr.meta else {
                return None;
            };
            let syn::Expr::Lit(value) = &value.value else {
                return None;
            };
            let syn::Lit::Str(value) = &value.lit else {
                return None;
            };
            Some(value.value().trim().to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ambiguous_and_unsupported_signatures() {
        for source in [
            "fn command() {}",
            "fn hello<T>() {}",
            "unsafe fn hello() {}",
            "fn hello(#[arg(long = \"help\")] value: String) {}",
            "fn hello(#[arg(long = \"x\")] a: String, #[arg(long = \"x\")] b: String) {}",
            "fn hello(#[arg(short)] value: String) {}",
        ] {
            assert!(
                expand(quote!(), syn::parse_str(source).unwrap()).is_err(),
                "{source}"
            );
        }
    }
    #[test]
    fn expands_async_typed_defaults_and_context() {
        let output = expand(
            quote!(name = "hello"),
            syn::parse_quote!(
                async fn hello(
                    #[arg(long, default = "world")] name: String,
                    count: u32,
                    optional: Option<String>,
                    verbose: bool,
                    #[context] context: CommandContext,
                ) -> Result<String, String> {
                    todo!()
                }
            ),
        )
        .unwrap();
        syn::parse2::<syn::File>(output).unwrap();
    }
}
