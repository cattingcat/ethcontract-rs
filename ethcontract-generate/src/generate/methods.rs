use crate::generate::{types, Context};
use crate::util;
use anyhow::{anyhow, Context as _, Result};
use ethcontract_common::abi::{Function, Param, StateMutability};
use ethcontract_common::abiext::FunctionExt;
use ethcontract_common::hash::H32;
use inflector::Inflector;
use proc_macro2::{Literal, TokenStream};
use quote::quote;
use syn::Ident;

pub(crate) fn expand(cx: &Context) -> Result<TokenStream> {
    let functions = expand_functions(cx)?;
    let fallback = expand_fallback(cx);

    Ok(quote! {
        #functions
        #fallback
    })
}

/// Expands a context into a method struct containing all the generated bindings
/// to the Solidity contract methods.
fn expand_functions(cx: &Context) -> Result<TokenStream> {
    let mut aliases = cx.method_aliases.clone();
    let functions = cx
        .contract
        .abi
        .functions()
        .map(|function| {
            let signature = function.abi_signature();

            let alias = aliases.remove(&signature);
            let name = alias.unwrap_or_else(|| util::safe_ident(&function.name.to_snake_case()));
            let signature = function.abi_signature();
            let selector = expand_selector(function.selector());
            let inputs = expand_inputs(&function.inputs)
                .with_context(|| format!("error expanding function '{}'", signature))?;
            let input_types = expand_input_types(&function.inputs)
                .with_context(|| format!("error expanding function '{}'", signature))?;
            let outputs = expand_outputs(&function.outputs)
                .with_context(|| format!("error expanding function '{}'", signature))?;

            Ok((function, name, selector, inputs, input_types, outputs))
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(unused) = aliases.keys().next() {
        return Err(anyhow!(
            "a manual method alias for '{}' was specified but this method does not exist",
            unused,
        ));
    }

    let methods = functions
        .iter()
        .map(|(function, name, selector, inputs, _, outputs)| {
            expand_function(cx, function, name, selector, inputs, outputs)
        });

    let methods_attrs = quote! { #[derive(Clone)] };
    let methods_struct = quote! {
        struct Methods {
            instance: self::ethcontract::dyns::DynInstance,
        }
    };

    let signature_accessors =
        functions
            .iter()
            .map(|(function, name, selector, _, input_types, outputs)| {
                expand_signature_accessor(function, name, selector, input_types, outputs)
            });

    let signatures_attrs = quote! { #[derive(Clone, Copy)] };
    let signatures_struct = quote! {
        struct Signatures;
    };

    if functions.is_empty() {
        // NOTE: The methods struct is still needed when there are no functions
        //   as it contains the the runtime instance. The code is setup this way
        //   so that the contract can implement `Deref` targeting the methods
        //   struct and, therefore, call the methods directly.
        return Ok(quote! {
            #methods_attrs
            #methods_struct

            #signatures_attrs
            #signatures_struct
        });
    }

    Ok(quote! {
        impl Contract {
            /// Returns an object that allows accessing typed method signatures.
            pub fn signatures() -> Signatures {
                Signatures
            }

            /// Retrieves a reference to type containing all the generated
            /// contract methods. This can be used for methods where the name
            /// would collide with a common method (like `at` or `deployed`).
            pub fn methods(&self) -> &Methods {
                &self.methods
            }
        }

        /// Type containing signatures for all methods for generated contract type.
        #signatures_attrs
        pub #signatures_struct

        impl Signatures {
            #( #signature_accessors )*
        }

        /// Type containing all contract methods for generated contract type.
        #methods_attrs
        pub #methods_struct

        #[allow(clippy::too_many_arguments, clippy::type_complexity)]
        impl Methods {
            #( #methods )*
        }

        impl std::ops::Deref for Contract {
            type Target = Methods;
            fn deref(&self) -> &Self::Target {
                &self.methods
            }
        }
    })
}

fn expand_function(
    cx: &Context,
    function: &Function,
    name: &Ident,
    selector: &TokenStream,
    inputs: &TokenStream,
    outputs: &TokenStream,
) -> TokenStream {
    let signature = function.abi_signature();

    let doc_str = cx
        .contract
        .devdoc
        .methods
        .get(&signature)
        .or_else(|| cx.contract.userdoc.methods.get(&signature))
        .and_then(|entry| entry.details.as_ref())
        .map(String::as_str)
        .unwrap_or("Generated by `ethcontract`");
    let doc = util::expand_doc(doc_str);

    let (method, result_type_name) = match function.state_mutability {
        StateMutability::Pure | StateMutability::View => {
            (quote! { view_method }, quote! { DynViewMethodBuilder })
        }
        _ => (quote! { method }, quote! { DynMethodBuilder }),
    };
    let result = quote! { self::ethcontract::dyns::#result_type_name<#outputs> };
    let arg = expand_inputs_call_arg(&function.inputs);

    quote! {
        #doc
        pub fn #name(&self #inputs) -> #result {
            self.instance.#method(#selector, #arg)
                .expect("generated call")
        }
    }
}

fn expand_signature_accessor(
    function: &Function,
    name: &Ident,
    selector: &TokenStream,
    input_types: &TokenStream,
    outputs: &TokenStream,
) -> TokenStream {
    let doc = util::expand_doc(&format!(
        "Returns signature for method `{}`.",
        function.signature()
    ));
    quote! {
        #doc
        #[allow(clippy::type_complexity)]
        pub fn #name(&self) -> self::ethcontract::contract::Signature<#input_types, #outputs> {
            self::ethcontract::contract::Signature::new(#selector)
        }
    }
}

pub(crate) fn expand_inputs(inputs: &[Param]) -> Result<TokenStream> {
    let params = inputs
        .iter()
        .enumerate()
        .map(|(i, param)| {
            let name = util::expand_input_name(i, &param.name);
            let kind = types::expand(&param.kind)?;
            Ok(quote! { #name: #kind })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(quote! { #( , #params )* })
}

pub(crate) fn expand_input_types(inputs: &[Param]) -> Result<TokenStream> {
    let params = inputs
        .iter()
        .map(|param| types::expand(&param.kind))
        .collect::<Result<Vec<_>>>()?;
    Ok(quote! { ( #( #params ,)* ) })
}

pub(crate) fn expand_inputs_call_arg(inputs: &[Param]) -> TokenStream {
    let names = inputs
        .iter()
        .enumerate()
        .map(|(i, param)| util::expand_input_name(i, &param.name));
    quote! { ( #( #names ,)* ) }
}

fn expand_outputs(outputs: &[Param]) -> Result<TokenStream> {
    match outputs.len() {
        0 => Ok(quote! { () }),
        1 => types::expand(&outputs[0].kind),
        _ => {
            let types = outputs
                .iter()
                .map(|param| types::expand(&param.kind))
                .collect::<Result<Vec<_>>>()?;
            Ok(quote! { (#( #types ),*) })
        }
    }
}

fn expand_selector(selector: H32) -> TokenStream {
    let bytes = selector.iter().copied().map(Literal::u8_unsuffixed);
    quote! { [#( #bytes ),*] }
}

/// Expands a context into fallback method when the contract implements one,
/// and an empty token stream otherwise.
fn expand_fallback(cx: &Context) -> TokenStream {
    if cx.contract.abi.fallback || cx.contract.abi.receive {
        quote! {
            impl Contract {
                /// Returns a method builder to setup a call to a smart
                /// contract's fallback function.
                pub fn fallback<D>(&self, data: D) -> self::ethcontract::dyns::DynMethodBuilder<()>
                where
                    D: Into<Vec<u8>>,
                {
                    self.raw_instance().fallback(data)
                        .expect("generated fallback method")
                }
            }
        }
    } else {
        quote! {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethcontract_common::abi::ParamType;

    #[test]
    fn expand_inputs_empty() {
        assert_quote!(expand_inputs(&[]).unwrap().to_string(), {},);
    }

    #[test]
    fn expand_inputs_() {
        assert_quote!(
            expand_inputs(

                &[
                    Param {
                        name: "a".to_string(),
                        kind: ParamType::Bool,
                    },
                    Param {
                        name: "b".to_string(),
                        kind: ParamType::Address,
                    },
                ],
            )
            .unwrap(),
            { , a: bool, b: self::ethcontract::Address },
        );
    }

    #[test]
    fn expand_outputs_empty() {
        assert_quote!(expand_outputs(&[],).unwrap(), { () });
    }

    #[test]
    fn expand_outputs_single() {
        assert_quote!(
            expand_outputs(&[Param {
                name: "a".to_string(),
                kind: ParamType::Bool,
            }])
            .unwrap(),
            { bool },
        );
    }

    #[test]
    fn expand_outputs_multiple() {
        assert_quote!(
            expand_outputs(&[
                Param {
                    name: "a".to_string(),
                    kind: ParamType::Bool,
                },
                Param {
                    name: "b".to_string(),
                    kind: ParamType::Address,
                },
            ],)
            .unwrap(),
            { (bool, self::ethcontract::Address) },
        );
    }
}
