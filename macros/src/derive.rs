mod context;

use proc_macro2::TokenStream;
use quote::quote_spanned;
use syn::{
    parse_quote, DataEnum, DataStruct, DeriveInput, Fields, FieldsNamed, GenericParam, Generics,
    Ident, ItemImpl,
};

use self::context::Container;

pub fn derive(input: DeriveInput) -> Result<ItemImpl, syn::Error> {
    let ctx = context::Container::from_input(&input)?;

    let ident = input.ident;

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut impl_generics: Generics = parse_quote! {#impl_generics};
    for param in impl_generics.params.iter_mut() {
        if let GenericParam::Type(ty) = param {
            // We add the `JsonTypedef` bound to every type parameter.
            // This isn't always correct, but it's an okay-ish heuristic.
            ty.bounds.push(parse_quote! { ::jtd_derive::JsonTypedef });
        }
    }

    let type_params = input.generics.type_params().map(|p| &p.ident);
    let const_params = input.generics.const_params().map(|p| &p.ident);

    let res = match input.data {
        syn::Data::Struct(s) => gen_struct_schema(&ctx, &ident, s)?,
        syn::Data::Enum(e) => gen_enum_schema(&ctx, &ident, e)?,
        syn::Data::Union(_) => {
            quote_spanned! {ident.span()=> compile_error!("jtd-derive does not support unions")}
        }
    };

    Ok(parse_quote! {
        impl #impl_generics ::jtd_derive::JsonTypedef for #ident #ty_generics #where_clause {
            fn schema(gen: &mut ::jtd_derive::gen::Generator) -> ::jtd_derive::schema::Schema {
                use ::jtd_derive::JsonTypedef;
                use ::jtd_derive::schema::{Schema, SchemaType};
                #res
            }

            fn referenceable() -> bool {
                true
            }

            fn names() -> ::jtd_derive::schema::Names {
                ::jtd_derive::schema::Names {
                    short: stringify!(#ident),
                    long: concat!(module_path!(), "::", stringify!(#ident)),
                    type_params: [#(#type_params::names()),*].into(),
                    const_params: [#(#const_params.to_string()),*].into(),
                }
            }
        }
    })
}

fn gen_struct_schema(
    _ctx: &Container,
    ident: &Ident,
    s: DataStruct,
) -> Result<TokenStream, syn::Error> {
    match s.fields {
        Fields::Named(_) if s.fields.is_empty() => Err(syn::Error::new_spanned(
            ident,
            "jtd-derive does not support empty cstruct-like structs",
        )),

        Fields::Named(fields) => Ok(gen_named_fields(&fields, true)),
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let ty = &fields.unnamed[0].ty;

            Ok(parse_quote! {
                gen.sub_schema::<#ty>()
            })
        }
        Fields::Unnamed(_) => Err(syn::Error::new_spanned(
            ident,
            "jtd-derive only supports tuple structs if they have exactly one field",
        )),
        _ => Err(syn::Error::new_spanned(
            ident,
            "jtd-derive does not support unit structs",
        )),
    }
}

fn gen_enum_schema(
    ctx: &Container,
    ident: &Ident,
    enu: DataEnum,
) -> Result<TokenStream, syn::Error> {
    match enum_kind(ident, &enu)? {
        EnumKind::UnitVariants => {
            let idents = enu.variants.iter().map(|v| &v.ident);

            let enum_schema = parse_quote! {
                Schema {
                    ty: SchemaType::Enum {
                        r#enum: [#(stringify!(#idents)),*].into(),
                    },
                    ..::jtd_derive::schema::Schema::default()
                }
            };

            match &ctx.tag_type {
                context::TagType::External => Ok(enum_schema),
                context::TagType::Internal(tag) => Ok(parse_quote! {
                    Schema {
                        ty: SchemaType::Properties {
                            properties: [
                                (#tag, #enum_schema)
                            ].into(),
                            additional_properties: true,
                            optional_properties: [].into(),
                        },
                        ..::jtd_derive::schema::Schema::default()
                    }
                }),
            }
        }
        EnumKind::StructVariants => {
            let tag = match &ctx.tag_type {
                context::TagType::External => {
                    return Err(syn::Error::new_spanned(
                        ident,
                        "jtd-derive requires an enum with struct variants to have a tag",
                    ));
                }
                context::TagType::Internal(t) => t,
            };

            let (idents, variants): (Vec<_>, Vec<_>) = enu
                .variants
                .iter()
                .map(|v| {
                    (
                        &v.ident,
                        gen_named_fields(unwrap_fields_named(&v.fields), true),
                    )
                })
                .unzip();

            Ok(parse_quote! {
                Schema {
                    ty: SchemaType::Discriminator {
                        discriminator: #tag,
                        mapping: [#((stringify!(#idents), #variants)),*].into(),
                    },
                    ..::jtd_derive::schema::Schema::default()
                }
            })
        }
    }
}

fn gen_named_fields(fields: &FieldsNamed, additional: bool) -> TokenStream {
    let (idents, types): (Vec<_>, Vec<_>) = fields.named.iter().map(|f| (&f.ident, &f.ty)).unzip();

    parse_quote! {
        Schema {
            ty: SchemaType::Properties {
                properties: [#((stringify!(#idents), gen.sub_schema::<#types>())),*].into(),
                optional_properties: [].into(),
                additional_properties: #additional,
            },
            ..::jtd_derive::schema::Schema::default()
        }
    }
}

fn unwrap_fields_named(fields: &Fields) -> &FieldsNamed {
    if let Fields::Named(named) = fields {
        named
    } else {
        // this branch should never be reached, so it being a panic and not
        // a quoted compile_error is OK
        panic!("expected named fields")
    }
}

fn enum_kind(ident: &Ident, e: &DataEnum) -> Result<EnumKind, syn::Error> {
    let (mut named, mut unit) = (None, None);

    for variant in &e.variants {
        match variant.fields {
            Fields::Named(_) => {
                named = Some(variant);
                if unit.is_some() {
                    break;
                }
            }
            Fields::Unit => {
                unit = Some(variant);
                if named.is_some() {
                    break;
                }
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "Typedef can't support tuple variants",
                ))
            }
        }
    }

    match (named, unit) {
        (None, None) => Err(syn::Error::new_spanned(
            ident,
            "jtd-derive does not support empty enums",
        )),
        (None, Some(_)) => Ok(EnumKind::UnitVariants),
        (Some(_), None) => Ok(EnumKind::StructVariants),
        (Some(named), Some(unit)) => {
            let mut err = syn::Error::new_spanned(
                ident,
                "Typedef can't support enums with a mix of unit and struct variants",
            );

            // TODO: if the output looks like independent errors, we probably want
            // to scratch the two errors below. probably
            err.combine(syn::Error::new_spanned(
                unit,
                format!("here's a unit variant of `{}`", ident),
            ));
            err.combine(syn::Error::new_spanned(
                named,
                format!("here's a struct variant of `{}`", ident),
            ));

            Err(err)
        }
    }
}

enum EnumKind {
    // the enum only has unit variants
    UnitVariants,
    // the enum only has struct variants
    StructVariants,
}
