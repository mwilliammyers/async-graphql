use crate::args;
use crate::output_type::OutputType;
use crate::utils::{feature_block, get_crate_name, get_param_getter_ident, get_rustdoc};
use inflector::Inflector;
use proc_macro::TokenStream;
use quote::quote;
use syn::ext::IdentExt;
use syn::{
    Block, Error, FnArg, ImplItem, ItemImpl, Pat, Result, ReturnType, Type, TypeImplTrait,
    TypeReference,
};

pub fn generate(object_args: &args::Object, item_impl: &mut ItemImpl) -> Result<TokenStream> {
    let crate_name = get_crate_name(object_args.internal);
    let (self_ty, self_name) = match item_impl.self_ty.as_ref() {
        Type::Path(path) => (
            path,
            path.path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap(),
        ),
        _ => return Err(Error::new_spanned(&item_impl.self_ty, "Invalid type")),
    };
    let generics = &item_impl.generics;
    let where_clause = &generics.where_clause;

    let gql_typename = object_args
        .name
        .clone()
        .unwrap_or_else(|| self_name.clone());

    let desc = object_args
        .desc
        .clone()
        .or_else(|| get_rustdoc(&item_impl.attrs).ok().flatten())
        .map(|s| quote! { Some(#s) })
        .unwrap_or_else(|| quote! {None});

    let mut create_stream = Vec::new();
    let mut schema_fields = Vec::new();

    for item in &mut item_impl.items {
        if let ImplItem::Method(method) = item {
            if let Some(field) = args::Field::parse(&crate_name, &method.attrs)? {
                let ident = &method.sig.ident;
                let field_name = field
                    .name
                    .clone()
                    .unwrap_or_else(|| method.sig.ident.unraw().to_string().to_camel_case());
                let field_desc = field
                    .desc
                    .as_ref()
                    .map(|s| quote! {Some(#s)})
                    .unwrap_or_else(|| quote! {None});
                let field_deprecation = field
                    .deprecation
                    .as_ref()
                    .map(|s| quote! {Some(#s)})
                    .unwrap_or_else(|| quote! {None});
                let features = field.features;

                if method.sig.asyncness.is_none() {
                    return Err(Error::new_spanned(
                        &method,
                        "The subscription stream function must be asynchronous",
                    ));
                }

                let ty = match &method.sig.output {
                    ReturnType::Type(_, ty) => OutputType::parse(ty)?,
                    ReturnType::Default => {
                        return Err(Error::new_spanned(&method.sig.output, "Missing type"))
                    }
                };

                let mut create_ctx = true;
                let mut args = Vec::new();

                for (idx, arg) in method.sig.inputs.iter_mut().enumerate() {
                    if let FnArg::Receiver(receiver) = arg {
                        if idx != 0 {
                            return Err(Error::new_spanned(
                                receiver,
                                "The self receiver must be the first parameter.",
                            ));
                        }
                    } else if let FnArg::Typed(pat) = arg {
                        if idx == 0 {
                            return Err(Error::new_spanned(
                                pat,
                                "The self receiver must be the first parameter.",
                            ));
                        }

                        match (&*pat.pat, &*pat.ty) {
                            (Pat::Ident(arg_ident), Type::Path(arg_ty)) => {
                                args.push((
                                    arg_ident.clone(),
                                    arg_ty.clone(),
                                    args::Argument::parse(&crate_name, &pat.attrs)?,
                                ));
                                pat.attrs.clear();
                            }
                            (arg, Type::Reference(TypeReference { elem, .. })) => {
                                if let Type::Path(path) = elem.as_ref() {
                                    if idx != 1
                                        || path.path.segments.last().unwrap().ident != "Context"
                                    {
                                        return Err(Error::new_spanned(
                                            arg,
                                            "The Context must be the second argument.",
                                        ));
                                    } else {
                                        create_ctx = false;
                                    }
                                }
                            }
                            _ => {
                                return Err(Error::new_spanned(arg, "Incorrect argument type"));
                            }
                        }
                    } else {
                        return Err(Error::new_spanned(arg, "Incorrect argument type"));
                    }
                }

                if create_ctx {
                    let arg =
                        syn::parse2::<FnArg>(quote! { _: &#crate_name::Context<'_> }).unwrap();
                    method.sig.inputs.insert(1, arg);
                }

                let mut schema_args = Vec::new();
                let mut use_params = Vec::new();
                let mut get_params = Vec::new();

                for (
                    ident,
                    ty,
                    args::Argument {
                        name,
                        desc,
                        default,
                        validator,
                        ..
                    },
                ) in args
                {
                    let name = name
                        .clone()
                        .unwrap_or_else(|| ident.ident.unraw().to_string().to_camel_case());
                    let desc = desc
                        .as_ref()
                        .map(|s| quote! {Some(#s)})
                        .unwrap_or_else(|| quote! {None});
                    let schema_default = default
                        .as_ref()
                        .map(|value| {
                            quote! {Some( <#ty as #crate_name::InputValueType>::to_value(&#value).to_string() )}
                        })
                        .unwrap_or_else(|| quote! {None});

                    schema_args.push(quote! {
                        args.insert(#name, #crate_name::registry::MetaInputValue {
                            name: #name,
                            description: #desc,
                            ty: <#ty as #crate_name::Type>::create_type_info(registry),
                            default_value: #schema_default,
                            validator: #validator,
                        });
                    });

                    use_params.push(quote! { #ident });

                    let default = match default {
                        Some(default) => quote! { Some(|| -> #ty { #default }) },
                        None => quote! { None },
                    };
                    let param_getter_name = get_param_getter_ident(&ident.ident.to_string());
                    get_params.push(quote! {
                        let #param_getter_name = || -> #crate_name::Result<#ty> { ctx.param_value(#name, #default) };
                        let #ident: #ty = ctx.param_value(#name, #default)?;
                    });
                }

                let res_ty = ty.value_type();
                let stream_ty = if let Type::ImplTrait(TypeImplTrait { bounds, .. }) = &res_ty {
                    quote! { #bounds }
                } else {
                    quote! { #res_ty }
                };

                if let OutputType::Value(inner_ty) = &ty {
                    let block = &method.block;
                    let new_block = quote!({
                        {
                            let value = (move || { async move #block })().await;
                            Ok(value)
                        }
                    });
                    method.block = syn::parse2::<Block>(new_block).expect("invalid block");
                    method.sig.output = syn::parse2::<ReturnType>(
                        quote! { -> #crate_name::FieldResult<#inner_ty> },
                    )
                    .expect("invalid result type");
                }

                method.block =
                    syn::parse2::<Block>(feature_block(&crate_name, &features, &field_name, {
                        let block = &method.block;
                        quote! { #block }
                    }))
                    .expect("invalid block");

                schema_fields.push(quote! {
                    fields.insert(#field_name.to_string(), #crate_name::registry::MetaField {
                        name: #field_name.to_string(),
                        description: #field_desc,
                        args: {
                            let mut args = #crate_name::indexmap::IndexMap::new();
                            #(#schema_args)*
                            args
                        },
                        ty: <<#stream_ty as #crate_name::futures::stream::Stream>::Item as #crate_name::Type>::create_type_info(registry),
                        deprecation: #field_deprecation,
                        cache_control: Default::default(),
                        external: false,
                        requires: None,
                        provides: None,
                    });
                });

                let create_field_stream = quote! {
                    self.#ident(ctx, #(#use_params),*)
                        .await
                        .map_err(|err| {
                            err.into_error_with_path(ctx.item.pos, ctx.path_node.as_ref())
                        })?
                };

                let guard = field.guard.map(|guard| quote! {
                    #guard.check(ctx).await.map_err(|err| err.into_error_with_path(ctx.item.pos, ctx.path_node.as_ref()))?;
                });
                if field.post_guard.is_some() {
                    return Err(Error::new_spanned(
                        method,
                        "The subscription field does not support post guard",
                    ));
                }

                let stream_fn = quote! {
                    #(#get_params)*
                    #guard
                    let field_name = ::std::sync::Arc::new(ctx.item.node.response_key().node.clone());
                    let field = ::std::sync::Arc::new(ctx.item.clone());

                    let pos = ctx.item.pos;
                    let schema_env = ctx.schema_env.clone();
                    let query_env = ctx.query_env.clone();
                    let stream = #crate_name::futures::StreamExt::then(#create_field_stream, {
                        let field_name = field_name.clone();
                        move |msg| {
                            let schema_env = schema_env.clone();
                            let query_env = query_env.clone();
                            let field = field.clone();
                            let field_name = field_name.clone();
                            async move {
                                let resolve_id = ::std::sync::atomic::AtomicUsize::default();
                                let ctx_selection_set = query_env.create_context(
                                    &schema_env,
                                    Some(#crate_name::QueryPathNode {
                                        parent: None,
                                        segment: #crate_name::QueryPathSegment::Name(&field_name),
                                    }),
                                    &field.node.selection_set,
                                    &resolve_id,
                                );
                                #crate_name::OutputValueType::resolve(&msg, &ctx_selection_set, &*field)
                                    .await
                                    .map(|value| {
                                        #crate_name::serde_json::json!({
                                            field_name.as_str(): value
                                        })
                                    })
                            }
                        }
                    });
                    #crate_name::Result::Ok(#crate_name::futures::StreamExt::scan(
                        stream,
                        false,
                        |errored, item| {
                            if *errored {
                                return #crate_name::futures::future::ready(None);
                            }
                            if item.is_err() {
                                *errored = true;
                            }
                            #crate_name::futures::future::ready(Some(item))
                        },
                    ))
                };

                create_stream.push(quote! {
                    if ctx.item.node.name.node == #field_name {
                        return ::std::boxed::Box::pin(
                            #crate_name::futures::TryStreamExt::try_flatten(
                                #crate_name::futures::stream::once((move || async move { #stream_fn })())
                            )
                        );
                    }
                });
            }

            if let Some((idx, _)) = method
                .attrs
                .iter()
                .enumerate()
                .find(|(_, a)| a.path.is_ident("field"))
            {
                method.attrs.remove(idx);
            }
        }
    }

    let expanded = quote! {
        #item_impl

        #[allow(clippy::all, clippy::pedantic)]
        impl #generics #crate_name::Type for #self_ty #where_clause {
            fn type_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(#gql_typename)
            }

            #[allow(bare_trait_objects)]
            fn create_type_info(registry: &mut #crate_name::registry::Registry) -> String {
                registry.create_type::<Self, _>(|registry| #crate_name::registry::MetaType::Object {
                    name: #gql_typename.to_string(),
                    description: #desc,
                    fields: {
                        let mut fields = #crate_name::indexmap::IndexMap::new();
                        #(#schema_fields)*
                        fields
                    },
                    cache_control: ::std::default::Default::default(),
                    extends: false,
                    keys: None,
                })
            }
        }

        #[allow(clippy::all, clippy::pedantic)]
        #[allow(unused_braces, unused_variables)]
        impl #crate_name::SubscriptionType for #self_ty #where_clause {
            fn create_field_stream<'a>(
                &'a self,
                ctx: &'a #crate_name::Context<'a>,
            ) -> ::std::pin::Pin<::std::boxed::Box<dyn #crate_name::futures::Stream<Item = #crate_name::Result<#crate_name::serde_json::Value>> + Send + 'a>> {
                #(#create_stream)*
                let error = #crate_name::QueryError::FieldNotFound {
                    field_name: ctx.item.node.name.to_string(),
                    object: #gql_typename.to_string(),
                }
                    .into_error(ctx.item.pos);
                ::std::boxed::Box::pin(#crate_name::futures::stream::once(async { Err(error) }))
            }
        }
    };
    Ok(expanded.into())
}
