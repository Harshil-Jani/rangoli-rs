//! `#[derive(Model)]` for Rangoli. Everything the admin, migrations and
//! query builder need is generated here as `'static` metadata, so the rest of
//! the framework never needs runtime reflection.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields, GenericArgument, LitInt, LitStr, PathArguments, Type};

#[proc_macro_derive(Model, attributes(model, field))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand(input: DeriveInput) -> syn::Result<Tokens> {
    let ident = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&input.generics, "Model cannot be generic"));
    }
    let mut table = ident.to_string().to_lowercase();
    let mut display: Option<String> = None;
    let mut m2m: Vec<(syn::Ident, syn::Path)> = vec![];
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("model")) {
        attr.parse_nested_meta(|m| {
            if m.path.is_ident("table") {
                table = m.value()?.parse::<LitStr>()?.value();
            } else if m.path.is_ident("display") {
                display = Some(m.value()?.parse::<LitStr>()?.value());
            } else if m.path.is_ident("m2m") {
                m.parse_nested_meta(|rel| {
                    let name = rel.path.get_ident().cloned().ok_or_else(|| rel.error("expected `name = Model`"))?;
                    m2m.push((name, rel.value()?.parse()?));
                    Ok(())
                })?;
            } else {
                return Err(m.error("expected `table`, `display` or `m2m(name = Model)`"));
            }
            Ok(())
        })?;
    }

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(ident, "Model can only be derived for structs"));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(ident, "Model needs named fields"));
    };

    let mut has_id = false;
    let (mut metas, mut reads, mut values, mut cols, mut touches) = (vec![], vec![], vec![], vec![], vec![]);
    for f in &fields.named {
        let name = f.ident.as_ref().unwrap();
        let name_s = name.to_string();
        let (inner, null) = unwrap_option(&f.ty);
        let base = base_type(inner)?;

        let (mut max_length, mut text, mut unique, mut password, mut cascade, mut fk) =
            (None::<u32>, false, false, false, false, None::<syn::Path>);
        let (mut auto_now, mut auto_now_add, mut index) = (false, false, false);
        for attr in f.attrs.iter().filter(|a| a.path().is_ident("field")) {
            attr.parse_nested_meta(|m| {
                if m.path.is_ident("max_length") {
                    max_length = Some(m.value()?.parse::<LitInt>()?.base10_parse()?);
                } else if m.path.is_ident("text") {
                    text = true;
                } else if m.path.is_ident("unique") {
                    unique = true;
                } else if m.path.is_ident("password") {
                    password = true;
                } else if m.path.is_ident("index") {
                    index = true;
                } else if m.path.is_ident("auto_now") {
                    auto_now = true;
                } else if m.path.is_ident("auto_now_add") {
                    auto_now_add = true;
                } else if m.path.is_ident("cascade") {
                    cascade = true;
                } else if m.path.is_ident("fk") {
                    fk = Some(m.value()?.parse()?);
                } else {
                    return Err(m.error(
                        "expected `max_length`, `text`, `unique`, `password`, `fk`, `cascade`, `index`, `auto_now` or `auto_now_add`",
                    ));
                }
                Ok(())
            })?;
        }

        if name_s == "id" {
            if !(null && base == "i64") {
                return Err(syn::Error::new_spanned(&f.ty, "`id` must be `Option<i64>` (it is None until the row is saved)"));
            }
            has_id = true;
            continue;
        }
        if fk.is_some() && base != "i64" {
            return Err(syn::Error::new_spanned(&f.ty, "a `fk` field must be `i64` or `Option<i64>`"));
        }
        if (auto_now || auto_now_add) && (base != "DateTime" || null) {
            return Err(syn::Error::new_spanned(&f.ty, "`auto_now` and `auto_now_add` need a `DateTime` field (not an Option)"));
        }
        if auto_now {
            touches.push(quote!(self.#name = ::rangoli::DateTime::now();));
        } else if auto_now_add {
            touches.push(quote!(if adding { self.#name = ::rangoli::DateTime::now(); }));
        }
        if cascade && fk.is_none() {
            return Err(syn::Error::new_spanned(name, "`cascade` needs `fk = Model`"));
        }
        if (text || max_length.is_some() || password) && base != "String" {
            return Err(syn::Error::new_spanned(&f.ty, "`text`, `max_length` and `password` only apply to String fields"));
        }

        let ty = match base {
            "i64" => quote!(Int),
            "f64" => quote!(Float),
            "bool" => quote!(Bool),
            "DateTime" => quote!(DateTime),
            _ if text => quote!(Text),
            _ => {
                let n = max_length.unwrap_or(255);
                quote!(Varchar(#n))
            }
        };
        let fk_tokens = match &fk {
            Some(p) => quote!(Some(<#p as ::rangoli::orm::Model>::TABLE)),
            None => quote!(None),
        };
        metas.push(quote! {
            ::rangoli::orm::FieldMeta {
                name: #name_s, ty: ::rangoli::orm::FieldType::#ty, null: #null,
                unique: #unique, password: #password, fk: #fk_tokens, cascade: #cascade,
                auto_now: #auto_now, auto_now_add: #auto_now_add, index: #index,
            }
        });
        reads.push(quote! {
            #name: ::rangoli::orm::FromValue::from_value(::rangoli::orm::read(row, #name_s, ::rangoli::orm::FieldType::#ty)?)?
        });
        values.push(quote!(::rangoli::orm::Value::from(::core::clone::Clone::clone(&self.#name))));
        let col = format_ident!("{}", name_s.to_uppercase());
        cols.push(quote!(pub const #col: ::rangoli::orm::Col<Self, #inner> = ::rangoli::orm::Col::new(#name_s);));
    }
    if !has_id {
        return Err(syn::Error::new_spanned(ident, "a Model needs `pub id: Option<i64>`"));
    }

    let (mut m2m_metas, mut m2m_items) = (vec![], vec![]);
    for (name, target) in &m2m {
        let (name_s, through) = (name.to_string(), format!("{table}_{name}"));
        let konst = format_ident!("{}", name_s.to_uppercase());
        m2m_metas.push(quote! {
            ::rangoli::orm::M2mMeta { name: #name_s, through: #through, target: <#target as ::rangoli::orm::Model>::TABLE }
        });
        m2m_items.push(quote! {
            pub const #konst: ::rangoli::orm::M2m<Self, #target> = ::rangoli::orm::M2m::new(#through);
            pub fn #name(&self) -> ::rangoli::orm::Related<Self, #target> { Self::#konst.of(self) }
        });
    }

    let name_s = ident.to_string();
    let display = match display {
        Some(d) => quote!(Some(#d)),
        None => quote!(None),
    };
    Ok(quote! {
        impl ::rangoli::orm::Model for #ident {
            const TABLE: &'static str = #table;
            fn meta() -> &'static ::rangoli::orm::ModelMeta {
                static META: ::rangoli::orm::ModelMeta = ::rangoli::orm::ModelMeta {
                    name: #name_s, table: #table, display: #display, fields: &[#(#metas),*], m2m: &[#(#m2m_metas),*],
                };
                &META
            }
            fn from_row(row: &::rangoli::orm::AnyRow) -> ::rangoli::Result<Self> {
                Ok(Self {
                    id: ::rangoli::orm::FromValue::from_value(::rangoli::orm::read(row, "id", ::rangoli::orm::FieldType::Int)?)?,
                    #(#reads),*
                })
            }
            fn pk(&self) -> Option<i64> { self.id }
            fn set_pk(&mut self, id: i64) { self.id = Some(id); }
            fn values(&self) -> Vec<::rangoli::orm::Value> { vec![#(#values),*] }
            #[allow(unused_variables)]
            fn before_save(&mut self, adding: bool) { #(#touches)* }
        }
        impl #ident {
            pub const ID: ::rangoli::orm::Col<Self, i64> = ::rangoli::orm::Col::new("id");
            #(#cols)*
            #(#m2m_items)*
        }
    })
}

fn unwrap_option(ty: &Type) -> (&Type, bool) {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            if seg.ident == "Option" {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(GenericArgument::Type(inner)) = args.args.first() {
                        return (inner, true);
                    }
                }
            }
        }
    }
    (ty, false)
}

fn base_type(ty: &Type) -> syn::Result<&'static str> {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            for t in ["i64", "f64", "bool", "String", "DateTime"] {
                if seg.ident == t {
                    return Ok(t);
                }
            }
        }
    }
    Err(syn::Error::new_spanned(ty, "supported field types: i64, f64, bool, String, DateTime (optionally wrapped in Option)"))
}
