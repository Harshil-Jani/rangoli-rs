//! `#[derive(Model)]` for Rangoli. Everything the admin, migrations and
//! query builder need is generated here as `'static` metadata, so the rest of
//! the framework never needs runtime reflection.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields, GenericArgument, Lit, LitInt, LitStr, PathArguments, Type};

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
    let mut indexes: Vec<Vec<String>> = vec![];
    let mut search: Vec<String> = vec![];
    let mut uniques: Vec<Vec<String>> = vec![];
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("model")) {
        attr.parse_nested_meta(|m| {
            if m.path.is_ident("table") {
                table = m.value()?.parse::<LitStr>()?.value();
            } else if m.path.is_ident("display") {
                display = Some(m.value()?.parse::<LitStr>()?.value());
            } else if m.path.is_ident("index") {
                let mut cols = vec![];
                m.parse_nested_meta(|col| {
                    cols.push(col.path.get_ident().ok_or_else(|| col.error("expected a field name"))?.to_string());
                    Ok(())
                })?;
                indexes.push(cols);
            } else if m.path.is_ident("unique") {
                let mut cols = vec![];
                m.parse_nested_meta(|col| {
                    cols.push(col.path.get_ident().ok_or_else(|| col.error("expected a field name"))?.to_string());
                    Ok(())
                })?;
                uniques.push(cols);
            } else if m.path.is_ident("search") {
                m.parse_nested_meta(|col| {
                    search.push(col.path.get_ident().ok_or_else(|| col.error("expected a field name"))?.to_string());
                    Ok(())
                })?;
            } else if m.path.is_ident("m2m") {
                m.parse_nested_meta(|rel| {
                    let name = rel.path.get_ident().cloned().ok_or_else(|| rel.error("expected `name = Model`"))?;
                    m2m.push((name, rel.value()?.parse()?));
                    Ok(())
                })?;
            } else {
                return Err(m.error("expected `table`, `display`, `index(a, b)`, `unique(a, b)`, `search(a, b)` or `m2m(name = Model)`"));
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
    let mut all_checks = vec![];
    let mut builds = vec![];
    for f in &fields.named {
        let name = f.ident.as_ref().unwrap();
        let name_s = name.to_string();
        let (inner, null) = unwrap_option(&f.ty);

        let (mut max_length, mut text, mut unique, mut password, mut cascade, mut fk) =
            (None::<u32>, false, false, false, false, None::<syn::Path>);
        let (mut private, mut encrypted) = (false, false);
        let (mut auto_now, mut auto_now_add, mut auto, mut index, mut choices) = (false, false, false, false, false);
        let mut default: Option<Tokens> = None;
        let mut vector_dims: Option<u32> = None;
        for attr in f.attrs.iter().filter(|a| a.path().is_ident("field")) {
            attr.parse_nested_meta(|m| {
                if m.path.is_ident("max_length") {
                    max_length = Some(m.value()?.parse::<LitInt>()?.base10_parse()?);
                } else if m.path.is_ident("vector") {
                    vector_dims = Some(m.value()?.parse::<LitInt>()?.base10_parse()?);
                } else if m.path.is_ident("text") {
                    text = true;
                } else if m.path.is_ident("unique") {
                    unique = true;
                } else if m.path.is_ident("password") {
                    password = true;
                } else if m.path.is_ident("private") {
                    private = true;
                } else if m.path.is_ident("encrypted") {
                    encrypted = true;
                } else if m.path.is_ident("index") {
                    index = true;
                } else if m.path.is_ident("choices") {
                    choices = true;
                } else if m.path.is_ident("default") {
                    default = Some(match m.value()?.parse::<Lit>()? {
                        Lit::Int(i) => {
                            let v: i64 = i.base10_parse()?;
                            quote!(::rangoli::orm::Lit::Int(#v))
                        }
                        Lit::Float(x) => {
                            let v: f64 = x.base10_parse()?;
                            quote!(::rangoli::orm::Lit::Float(#v))
                        }
                        Lit::Bool(b) => {
                            let v = b.value;
                            quote!(::rangoli::orm::Lit::Bool(#v))
                        }
                        Lit::Str(st) => quote!(::rangoli::orm::Lit::Str(#st)),
                        other => return Err(syn::Error::new_spanned(other, "default must be an integer, float, bool or string literal")),
                    });
                } else if m.path.is_ident("auto_now") {
                    auto_now = true;
                } else if m.path.is_ident("auto_now_add") {
                    auto_now_add = true;
                } else if m.path.is_ident("auto") {
                    auto = true;
                } else if m.path.is_ident("cascade") {
                    cascade = true;
                } else if m.path.is_ident("fk") {
                    fk = Some(m.value()?.parse()?);
                } else {
                    return Err(m.error(
                        "expected `max_length`, `text`, `unique`, `password`, `fk`, `cascade`, `index`, `choices`, `default`, `private`, `encrypted`, `vector`, `auto`, `auto_now` or `auto_now_add`",
                    ));
                }
                Ok(())
            })?;
        }

        // Choices fields hold any `#[derive(Choices)]` enum.
        let base = if choices { "choices" } else { base_type(inner)? };
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
        if auto && (base != "Uuid" || null) {
            return Err(syn::Error::new_spanned(&f.ty, "`auto` needs a `Uuid` field (not an Option)"));
        }
        if auto {
            // Filled with a v7 on insert unless the caller set one; shown read-only like auto_now_add.
            touches.push(quote!(if adding && self.#name == ::rangoli::Uuid::NIL { self.#name = ::rangoli::Uuid::new_v7(); }));
        }
        let framework_fills = auto_now_add || auto;
        if auto_now {
            touches.push(quote!(self.#name = ::rangoli::DateTime::now();));
        } else if auto_now_add {
            touches.push(quote!(if adding { self.#name = ::rangoli::DateTime::now(); }));
        }
        if cascade && fk.is_none() {
            return Err(syn::Error::new_spanned(name, "`cascade` needs `fk = Model`"));
        }
        if (text || password) && base != "String" || max_length.is_some() && !matches!(base, "String" | "choices") {
            return Err(syn::Error::new_spanned(&f.ty, "`text`, `max_length` and `password` only apply to String fields"));
        }

        let mut checks = quote!();
        if encrypted && base != "String" {
            return Err(syn::Error::new_spanned(&f.ty, "`encrypted` applies to String fields"));
        }
        let ty = match base {
            _ if encrypted => quote!(Encrypted),
            "i64" => quote!(Int),
            "f64" => quote!(Float),
            "bool" => quote!(Bool),
            "DateTime" => quote!(DateTime),
            "Date" => quote!(Date),
            "Uuid" => quote!(Uuid),
            // The scale lives in the type (`Decimal<2>`), so ask it.
            "Decimal" => quote!(Decimal(<#inner as ::rangoli::orm::Kind>::KIND.decimal_places())),
            "Json" => quote!(Json),
            "Vector" => match vector_dims {
                Some(n) if n > 0 && n <= 16_000 => quote!(Vector(#n)),
                _ => return Err(syn::Error::new_spanned(&f.ty, "a Vector field needs its size: #[field(vector = 384)] (1 to 16000)")),
            },
            "choices" => {
                let n = max_length.unwrap_or(32);
                // Every stored value must fit the column: checked when the crate compiles.
                checks = quote! {
                    const _: () = {
                        let all = <#inner as ::rangoli::orm::Choice>::CHOICES;
                        let mut i = 0;
                        while i < all.len() {
                            assert!(all[i].0.len() <= #n as usize, "a choice value is longer than the field's max_length");
                            i += 1;
                        }
                    };
                };
                quote!(Varchar(#n))
            }
            _ if text => quote!(Text),
            _ => {
                let n = max_length.unwrap_or(255);
                quote!(Varchar(#n))
            }
        };
        let choices_tokens = if choices { quote!(Some(<#inner as ::rangoli::orm::Choice>::CHOICES)) } else { quote!(None) };
        let default_tokens = match &default {
            Some(t) => quote!(Some(#t)),
            None => quote!(None),
        };
        all_checks.push(checks);
        let fk_tokens = match &fk {
            Some(p) => quote!(Some(<#p as ::rangoli::orm::Model>::TABLE)),
            None => quote!(None),
        };
        // `///` doc comments become the field's help text everywhere it's shown.
        let doc: Vec<String> = f
            .attrs
            .iter()
            .filter(|a| a.path().is_ident("doc"))
            .filter_map(|a| match &a.meta {
                syn::Meta::NameValue(nv) => match &nv.value {
                    syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(s.value().trim().to_string()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let help_text = doc.join(" ").trim().to_string();
        let help_tokens = if help_text.is_empty() { quote!(None) } else { quote!(Some(#help_text)) };
        metas.push(quote! {
            ::rangoli::orm::FieldMeta {
                name: #name_s, ty: ::rangoli::orm::FieldType::#ty, null: #null,
                unique: #unique, password: #password, private: #private, fk: #fk_tokens, cascade: #cascade,
                auto_now: #auto_now, auto_now_add: #framework_fills, index: #index,
                choices: #choices_tokens, default: #default_tokens, help: #help_tokens,
            }
        });
        reads.push(quote! {
            #name: ::rangoli::orm::FromValue::from_value(::rangoli::orm::read(row, #name_s, ::rangoli::orm::FieldType::#ty)?)?
        });
        builds.push(quote! {
            #name: ::rangoli::orm::FromValue::from_value(values.next().ok_or_else(|| ::rangoli::Error::Decode("too few values".into()))?)?
        });
        values.push(quote!(::rangoli::orm::Value::from(::core::clone::Clone::clone(&self.#name))));
        let col = format_ident!("{}", name_s.to_uppercase());
        cols.push(quote!(pub const #col: ::rangoli::orm::Col<Self, #inner> = ::rangoli::orm::Col::new(#name_s);));
        if let Some(target) = &fk {
            // `author_id` -> `Post::AUTHOR` and `post.author()`; a field without `_id` gets `<NAME>_REL`.
            let (konst, method) = match name_s.strip_suffix("_id") {
                Some(base) => (format_ident!("{}", base.to_uppercase()), format_ident!("{}", base)),
                None => (format_ident!("{}_REL", name_s.to_uppercase()), name.clone()),
            };
            cols.push(quote! {
                pub const #konst: ::rangoli::orm::Fk<Self, #target> = ::rangoli::orm::Fk::new(#name_s);
                /// The related row (one query); `Error::NotFound` if the key is null.
                pub fn #method(&self) -> impl ::core::future::Future<Output = ::rangoli::Result<#target>> + Send {
                    Self::#konst.get(self)
                }
            });
        }
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

    let field_names: Vec<String> = fields.named.iter().map(|f| f.ident.as_ref().unwrap().to_string()).collect();
    for cols in indexes.iter().chain(&uniques) {
        if let Some(bad) = cols.iter().find(|c| !field_names.contains(c) || *c == "id") {
            return Err(syn::Error::new_spanned(ident, format!("index(...)/unique(...) names unknown field `{bad}`")));
        }
    }
    let index_tokens = indexes.iter().map(|cols| quote!(&[#(#cols),*]));
    let unique_tokens = uniques.iter().map(|cols| quote!(&[#(#cols),*]));
    for col in &search {
        let text = fields.named.iter().find(|f| f.ident.as_ref().is_some_and(|i| i == col)).is_some_and(|f| {
            let ty = &f.ty;
            let s = quote!(#ty).to_string().replace(' ', "");
            s == "String" || s == "Option<String>"
        });
        if !text {
            return Err(syn::Error::new_spanned(ident, format!("search(...) needs text fields; `{col}` isn't a String field")));
        }
        let encrypted = fields
            .named
            .iter()
            .find(|f| f.ident.as_ref().is_some_and(|i| i == col))
            .is_some_and(|f| f.attrs.iter().any(|a| a.path().is_ident("field") && quote!(#a).to_string().contains("encrypted")));
        if encrypted {
            return Err(syn::Error::new_spanned(
                ident,
                format!("search(...) can't index `{col}`: it's encrypted, so the database only sees ciphertext"),
            ));
        }
        let private = fields
            .named
            .iter()
            .find(|f| f.ident.as_ref().is_some_and(|i| i == col))
            .is_some_and(|f| f.attrs.iter().any(|a| a.path().is_ident("field") && quote!(#a).to_string().contains("private")));
        if private {
            return Err(syn::Error::new_spanned(
                ident,
                format!("search(...) can't include `{col}`: it's private, and search results would reveal it"),
            ));
        }
    }

    let name_s = ident.to_string();
    let display = match display {
        Some(d) => quote!(Some(#d)),
        None => quote!(None),
    };
    Ok(quote! {
        #(#all_checks)*
        impl ::rangoli::orm::Model for #ident {
            const TABLE: &'static str = #table;
            fn meta() -> &'static ::rangoli::orm::ModelMeta {
                static META: ::rangoli::orm::ModelMeta = ::rangoli::orm::ModelMeta {
                    name: #name_s, table: #table, display: #display, fields: &[#(#metas),*], m2m: &[#(#m2m_metas),*],
                    indexes: &[#(#index_tokens),*],
                    search: &[#(#search),*],
                    unique_together: &[#(#unique_tokens),*],
                };
                &META
            }
            fn from_row(row: &::rangoli::orm::AnyRow) -> ::rangoli::Result<Self> {
                Ok(Self {
                    id: ::rangoli::orm::FromValue::from_value(::rangoli::orm::read(row, "id", ::rangoli::orm::FieldType::Int)?)?,
                    #(#reads),*
                })
            }
            fn from_values(id: Option<i64>, values: Vec<::rangoli::orm::Value>) -> ::rangoli::Result<Self> {
                #[allow(unused_mut, unused_variables)]
                let mut values = values.into_iter();
                Ok(Self { id, #(#builds),* })
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
            for t in ["i64", "f64", "bool", "String", "DateTime", "Date", "Decimal", "Uuid", "Json", "Vector"] {
                if seg.ident == t {
                    return Ok(t);
                }
            }
        }
    }
    Err(syn::Error::new_spanned(ty, "supported field types: i64, f64, bool, String, DateTime, Date, Decimal<P>, Uuid, Json, Vector, or a Choices enum with #[field(choices)] (optionally wrapped in Option)"))
}

/// `#[derive(Choices)]` on a fieldless enum: stored as a short string, shown with a label.
///
/// ```ignore
/// #[derive(Choices, Clone, Copy, Debug, PartialEq)]
/// enum Status { Draft, #[choice(label = "Live")] Published, #[choice(value = "old")] Archived }
/// ```
/// Stored values default to the snake_case variant name, labels to its words in sentence case.
#[proc_macro_derive(Choices, attributes(choice))]
pub fn derive_choices(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_choices(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand_choices(input: DeriveInput) -> syn::Result<Tokens> {
    let ident = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(ident, "Choices can only be derived for enums"));
    };
    let (mut pairs, mut to_db, mut from_db) = (vec![], vec![], vec![]);
    for v in &data.variants {
        if !matches!(v.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(v, "Choices variants cannot carry data"));
        }
        let words = split_words(&v.ident.to_string());
        let (mut value, mut label) = (words.join("_").to_lowercase(), sentence(&words));
        for attr in v.attrs.iter().filter(|a| a.path().is_ident("choice")) {
            attr.parse_nested_meta(|m| {
                if m.path.is_ident("value") {
                    value = m.value()?.parse::<LitStr>()?.value();
                } else if m.path.is_ident("label") {
                    label = m.value()?.parse::<LitStr>()?.value();
                } else {
                    return Err(m.error("expected `value` or `label`"));
                }
                Ok(())
            })?;
        }
        let var = &v.ident;
        pairs.push(quote!((#value, #label)));
        to_db.push(quote!(Self::#var => #value));
        from_db.push(quote!(#value => Some(Self::#var)));
    }
    let name_s = ident.to_string();
    Ok(quote! {
        impl ::rangoli::orm::Choice for #ident {
            const CHOICES: &'static [(&'static str, &'static str)] = &[#(#pairs),*];
            fn as_str(self) -> &'static str { match self { #(#to_db),* } }
            fn from_db(s: &str) -> Option<Self> { match s { #(#from_db,)* _ => None } }
        }
        impl ::core::convert::From<#ident> for ::rangoli::orm::Value {
            fn from(v: #ident) -> Self { ::rangoli::orm::Value::Text(::rangoli::orm::Choice::as_str(v).into()) }
        }
        impl ::rangoli::orm::FromValue for #ident {
            fn from_value(v: ::rangoli::orm::Value) -> ::rangoli::Result<Self> {
                match v {
                    ::rangoli::orm::Value::Text(s) => <Self as ::rangoli::orm::Choice>::from_db(&s)
                        .ok_or_else(|| ::rangoli::Error::Decode(format!("`{}` is not a {} choice", s, #name_s))),
                    other => Err(::rangoli::Error::Decode(format!("expected a {} choice, got {:?}", #name_s, other))),
                }
            }
        }
        impl ::rangoli::orm::Kind for #ident {
            const KIND: ::rangoli::orm::FieldType = ::rangoli::orm::FieldType::Text;
        }
        impl ::core::fmt::Display for #ident {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(::rangoli::orm::Choice::label(*self))
            }
        }
        impl ::rangoli::serde::Serialize for #ident {
            fn serialize<S: ::rangoli::serde::Serializer>(&self, s: S) -> ::core::result::Result<S::Ok, S::Error> {
                s.serialize_str(::rangoli::orm::Choice::as_str(*self))
            }
        }
        impl<'de> ::rangoli::serde::Deserialize<'de> for #ident {
            fn deserialize<D: ::rangoli::serde::Deserializer<'de>>(d: D) -> ::core::result::Result<Self, D::Error> {
                let s = <::std::string::String as ::rangoli::serde::Deserialize>::deserialize(d)?;
                <Self as ::rangoli::orm::Choice>::from_db(&s)
                    .ok_or_else(|| <D::Error as ::rangoli::serde::de::Error>::custom(format!("`{}` is not a valid {}", s, #name_s)))
            }
        }
    })
}

/// `PendingReview` -> ["Pending", "Review"]
fn split_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = vec![];
    for c in name.chars() {
        if c.is_uppercase() || words.is_empty() {
            words.push(String::new());
        }
        words.last_mut().unwrap().push(c);
    }
    words
}

/// ["Pending", "Review"] -> "Pending review"
fn sentence(words: &[String]) -> String {
    let s = words.join(" ").to_lowercase();
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Embed `migrations/*.json` (or the directory given, relative to the crate) in the binary:
/// `App::new().embed_migrations(rangoli::migrations!())`. Files are read at compile time, so
/// the deployed binary needs no migrations directory next to it.
#[proc_macro]
pub fn migrations(input: TokenStream) -> TokenStream {
    let dir = if input.is_empty() {
        "migrations".to_string()
    } else {
        match syn::parse::<syn::LitStr>(input) {
            Ok(l) => l.value(),
            Err(e) => return e.to_compile_error().into(),
        }
    };
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join(dir);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    files.sort();
    let pairs = files.iter().map(|p| {
        let name = p.file_stem().unwrap().to_string_lossy().into_owned();
        let path = p.to_string_lossy().into_owned();
        // include_str! makes cargo rebuild when an embedded file changes.
        quote!((#name, include_str!(#path)))
    });
    quote!(&[#(#pairs),*] as &'static [(&'static str, &'static str)]).into()
}
