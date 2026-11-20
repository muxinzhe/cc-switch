use heck::ToLowerCamelCase;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, FnArg, ItemFn, Pat, ReturnType, Type};

fn type_name(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(path) = ty else { return None };
    path.path.segments.last().map(|segment| &segment.ident)
}

#[proc_macro_attribute]
pub fn command(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let name_mode = attribute.to_string();
    let function = parse_macro_input!(item as ItemFn);
    let name = &function.sig.ident;
    let wrapper = format_ident!("ssh_{}", name);
    let mut arguments = Vec::new();
    let mut parameters = Vec::new();

    for arg in &function.sig.inputs {
        let FnArg::Typed(arg) = arg else { continue };
        let Pat::Ident(identifier) = arg.pat.as_ref() else { continue };
        let identifier = &identifier.ident;
        let arg_type = &arg.ty;

        let value = match type_name(arg_type).map(ToString::to_string).as_deref() {
            Some("State") => quote!(::tauri::Manager::try_state(&__ssh_app).ok_or_else(|| {
                ::serde_json::Value::String("Command state is unavailable".into())
            })?),
            Some("AppHandle") => quote!(__ssh_app.clone()),
            _ => {
                let key = if name_mode.contains("snake_case") {
                    identifier.to_string()
                } else {
                    identifier.to_string().to_lower_camel_case()
                };
                quote!(::serde_json::from_value(__ssh_payload.remove(#key).unwrap_or_default())
                    .map_err(|error| ::serde_json::Value::String(
                        format!("Invalid argument {}: {}", #key, error)
                    ))?)
            }
        };
        arguments.push(quote!(let #identifier: #arg_type = #value;));
        parameters.push(identifier);
    }

    let call = if function.sig.asyncness.is_some() {
        quote!(#name(#(#parameters),*).await)
    } else {
        quote!(::tauri::async_runtime::spawn_blocking(move || #name(#(#parameters),*)).await
            .map_err(|error| ::serde_json::Value::String(error.to_string()))?)
    };
    let result = match &function.sig.output {
        ReturnType::Type(_, ty) if type_name(ty).is_some_and(|name| name == "Result") => {
            quote!(let value = (#call).map_err(|error| {
                ::serde_json::to_value(error).unwrap_or_default()
            })?;)
        }
        _ => quote!(let value = #call;),
    };

    quote!(
        #function

        #[allow(dead_code, non_snake_case, unused_variables, unused_mut)]
        pub(crate) async fn #wrapper(
            __ssh_app: ::tauri::AppHandle,
            mut __ssh_payload: ::serde_json::Map<String, ::serde_json::Value>,
        ) -> Result<::serde_json::Value, ::serde_json::Value> {
            #(#arguments)*
            #result
            ::serde_json::to_value(value)
                .map_err(|error| ::serde_json::Value::String(error.to_string()))
        }
    )
    .into()
}
