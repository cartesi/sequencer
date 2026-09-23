use proc_macro::TokenStream;
use proc_macro2::TokenTree;
use quote::{quote, spanned::Spanned};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, ItemFn, LitStr, Result as SynResult,
};

#[derive(Debug, Default)]
struct Args {
    ignore: bool,
    kind: Option<LitStr>,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let mut pa = Args::default();

        while !input.is_empty() {
            let t = input.parse()?;
            match t {
                TokenTree::Ident(i) if i == "ignore" => pa.ignore = true,

                TokenTree::Ident(i) if i == "kind" => match input.parse()? {
                    TokenTree::Group(g) => {
                        let arr: Vec<_> = g.stream().into_iter().collect();
                        if arr.len() != 1 {
                            return Err(syn::Error::new(
                                g.span(),
                                "argument `kind` must have one argument",
                            ));
                        }

                        pa.kind = Some(syn::parse2(g.stream())?);
                    }

                    x => {
                        return Err(syn::Error::new(
                            x.span(),
                            format!("unrecognized kind `{x}`"),
                        ));
                    }
                },

                TokenTree::Punct(p) if p.as_char() == ',' => (),

                x => {
                    return Err(syn::Error::new(
                        x.span(),
                        format!("unrecognized argument `{x}`"),
                    ));
                }
            }
        }

        Ok(pa)
    }
}

#[proc_macro_attribute]
pub fn test_dapp(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed_args = parse_macro_input!(args as Args);
    let test_fn = parse_macro_input!(input as ItemFn);
    let name = &test_fn.sig.ident;
    let test_name = name.to_string();

    if !test_fn.sig.inputs.is_empty() {
        return syn::Error::new(
            test_fn.sig.__span(),
            format!("test function `{}` must have no arguments", name),
        )
        .into_compile_error()
        .into();
    }

    let ignore = parsed_args.ignore;
    let kind = quote_kind(parsed_args.kind.as_ref());

    let expanded = quote! {
        #test_fn

        testsi::inventory::submit! {
            testsi::TestCase { name: #test_name, function: #name, ignore: #ignore, kind: #kind  }
        }
    };

    TokenStream::from(expanded)
}

fn quote_kind(kind: Option<&LitStr>) -> proc_macro2::TokenStream {
    if let Some(kind) = kind {
        quote! { Some(#kind) }
    } else {
        quote! { None }
    }
}

#[cfg(test)]
mod tests {
    use super::quote_kind;
    use proc_macro2::Span;
    use syn::LitStr;

    #[test]
    fn quote_kind_defaults_to_none() {
        assert_eq!(quote_kind(None).to_string(), "None");
    }

    #[test]
    fn quote_kind_wraps_literal() {
        let kind = LitStr::new("dapp", Span::call_site());
        assert_eq!(quote_kind(Some(&kind)).to_string(), "Some (\"dapp\")");
    }
}
