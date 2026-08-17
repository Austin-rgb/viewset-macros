//! `#[derive(Entity)]`
//!
//! ```ignore
//! #[derive(Entity, sqlx::FromRow)]
//! #[entity(table = "products", create = "CreateProduct", update = "UpdateProduct", response = "ProductDto")]
//! pub struct Product {
//!     #[entity(pk)]
//!     id: Uuid,
//!     #[entity(searchable, sortable, filterable)]
//!     name: String,
//!     #[entity(sortable, filterable)]
//!     price: Decimal,
//!     #[entity(soft_delete)]
//!     deleted_at: Option<DateTime<Utc>>,
//! }
//! ```
//!
//! generates an `impl Entity for Product` wiring `TABLE`, `PK_COLUMN`,
//! `COLUMNS`, `SEARCHABLE`, `SORTABLE`, `FILTERABLE`,
//! `SOFT_DELETE_COLUMN` and `id()` from the annotations above. It does
//! *not* generate `CreateDto`/`UpdateDto`/`ResponseDto` themselves — those
//! are ordinary structs the developer writes (or derives separately);
//! this macro only wires the type names together.
//!
//! This is a design-level sketch of the macro's shape and behavior; treat
//! it as a starting point to compile and iterate on inside the actual
//! crate, not as pre-verified output.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident, LitStr};

#[proc_macro_derive(Entity, attributes(entity))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let container = ContainerAttrs::parse(&input);
    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(&input, "#[derive(Entity)] only supports structs")
            .to_compile_error()
            .into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(&input, "#[derive(Entity)] requires named fields")
            .to_compile_error()
            .into();
    };

    let mut pk_field: Option<Ident> = None;
    let mut pk_ty: Option<syn::Type> = None;
    let mut columns = Vec::new();
    let mut searchable = Vec::new();
    let mut sortable = Vec::new();
    let mut filterable = Vec::new();
    let mut soft_delete_col: Option<String> = None;
    let mut field_entries = Vec::new(); // (column name, SqlType tokens) for FIELDS

    for field in &fields.named {
        let ident = field.ident.clone().expect("named field");
        let col_name = ident.to_string();
        columns.push(col_name.clone());
        field_entries.push((col_name.clone(), sql_type_tokens(&field.ty)));

        let attrs = FieldAttrs::parse(field);
        if attrs.pk {
            pk_field = Some(ident.clone());
            pk_ty = Some(field.ty.clone());
        }
        if attrs.searchable {
            searchable.push(col_name.clone());
        }
        if attrs.sortable {
            sortable.push(col_name.clone());
        }
        if attrs.filterable {
            filterable.push(col_name.clone());
        }
        if attrs.soft_delete {
            soft_delete_col = Some(col_name.clone());
        }
    }

    let (pk_field, pk_ty) = match (pk_field, pk_ty) {
        (Some(f), Some(t)) => (f, t),
        _ => {
            // No explicit #[entity(pk)] — fall back to a field literally
            // named `id`, using *its* declared type rather than assuming
            // Uuid. Previously `type Id` was hardcoded to `uuid::Uuid`
            // regardless of the actual pk field's type, which silently
            // produced a non-compiling (or worse, semantically wrong)
            // `impl Entity` for any entity with a non-Uuid primary key
            // (i32/i64 auto-increment ids, etc.).
            match fields
                .named
                .iter()
                .find(|f| f.ident.as_ref().is_some_and(|i| i == "id"))
            {
                Some(f) => (f.ident.clone().expect("named field"), f.ty.clone()),
                None => {
                    return syn::Error::new_spanned(
                        &input,
                        "#[derive(Entity)] requires a field annotated #[entity(pk)], or a field named `id`",
                    )
                    .to_compile_error()
                    .into();
                }
            }
        }
    };
    let pk_column = pk_field.to_string();

    let table = container.table;
    let create_ty = container.create;
    let update_ty = container.update;
    let response_ty = container.response;

    let soft_delete_tokens = match soft_delete_col {
        Some(col) => quote! { Some(#col) },
        None => quote! { None },
    };

    let field_names = field_entries.iter().map(|(n, _)| n);
    let field_types = field_entries.iter().map(|(_, t)| t);

    let expanded = quote! {
        impl viewset::Entity for #name {
            type Id = #pk_ty;
            type CreateDto = #create_ty;
            type UpdateDto = #update_ty;
            type ResponseDto = #response_ty;

            const TABLE: &'static str = #table;
            const PK_COLUMN: &'static str = #pk_column;
            const COLUMNS: &'static [&'static str] = &[#(#columns),*];
            const FIELDS: &'static [viewset::Field] =
                &[#((#field_names, #field_types)),*];
            const SEARCHABLE: &'static [&'static str] = &[#(#searchable),*];
            const SORTABLE: &'static [&'static str] = &[#(#sortable),*];
            const FILTERABLE: &'static [&'static str] = &[#(#filterable),*];
            const SOFT_DELETE_COLUMN: Option<&'static str> = #soft_delete_tokens;

            fn id(&self) -> Self::Id {
                self.#pk_field.clone()
            }
        }
    };

    expanded.into()
}

/// Maps a field's Rust type to the `SqlType` used to bind it, peeling one
/// layer of `Option<T>` first (nullability is handled at the value level
/// via `SqlValue::Null`, not encoded in `SqlType` itself). Falls back to
/// `Json` for any type this table doesn't recognize — safer than guessing
/// wrong and binding, say, a struct as text.
fn sql_type_tokens(ty: &syn::Type) -> proc_macro2::TokenStream {
    let inner = unwrap_option(ty).unwrap_or(ty);
    let last_ident = match inner {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    };

    match last_ident.as_deref() {
        Some("String") | Some("str") => quote! { viewset::SqlType::Text },
        Some("i8") | Some("i16") | Some("i32") | Some("u8") | Some("u16") | Some("u32") => {
            quote! { viewset::SqlType::Int4 }
        }
        Some("i64") | Some("u64") | Some("isize") | Some("usize") => {
            quote! { viewset::SqlType::Int8 }
        }
        Some("f32") => quote! { viewset::SqlType::Float4 },
        Some("f64") => quote! { viewset::SqlType::Float8 },
        Some("bool") => quote! { viewset::SqlType::Bool },
        Some("Uuid") => quote! { viewset::SqlType::Uuid },
        Some("NaiveDate") => quote! { viewset::SqlType::Date },
        Some("NaiveDateTime") => quote! { viewset::SqlType::Timestamp },
        Some("DateTime") => quote! { viewset::SqlType::Timestamptz },
        Some("Decimal") => quote! { viewset::SqlType::Numeric },
        // serde_json::Value and anything unrecognized: treat as jsonb.
        _ => quote! { viewset::SqlType::Json },
    }
}

/// If `ty` is `Option<T>`, returns `T`; otherwise `None`.
fn unwrap_option(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

struct ContainerAttrs {
    table: String,
    create: Ident,
    update: Ident,
    response: Ident,
}

impl ContainerAttrs {
    fn parse(input: &DeriveInput) -> Self {
        let mut table = None;
        let mut create = None;
        let mut update = None;
        let mut response = None;

        for attr in &input.attrs {
            if !attr.path().is_ident("entity") {
                continue;
            }
            let _ = attr.parse_nested_meta(|meta| {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                if meta.path.is_ident("table") {
                    table = Some(lit.value());
                } else if meta.path.is_ident("create") {
                    create = Some(Ident::new(&lit.value(), lit.span()));
                } else if meta.path.is_ident("update") {
                    update = Some(Ident::new(&lit.value(), lit.span()));
                } else if meta.path.is_ident("response") {
                    response = Some(Ident::new(&lit.value(), lit.span()));
                }
                Ok(())
            });
        }

        Self {
            table: table.unwrap_or_else(|| input.ident.to_string().to_lowercase() + "s"),
            create: create.unwrap_or_else(|| input.ident.clone()),
            update: update.unwrap_or_else(|| input.ident.clone()),
            response: response.unwrap_or_else(|| input.ident.clone()),
        }
    }
}

#[derive(Default)]
struct FieldAttrs {
    pk: bool,
    searchable: bool,
    sortable: bool,
    filterable: bool,
    soft_delete: bool,
}

impl FieldAttrs {
    fn parse(field: &syn::Field) -> Self {
        let mut out = Self::default();
        for attr in &field.attrs {
            if !attr.path().is_ident("entity") {
                continue;
            }
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("pk") {
                    out.pk = true;
                } else if meta.path.is_ident("searchable") {
                    out.searchable = true;
                } else if meta.path.is_ident("sortable") {
                    out.sortable = true;
                } else if meta.path.is_ident("filterable") {
                    out.filterable = true;
                } else if meta.path.is_ident("soft_delete") {
                    out.soft_delete = true;
                }
                Ok(())
            });
        }
        out
    }
}
