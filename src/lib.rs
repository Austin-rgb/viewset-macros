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
//! `SOFT_DELETE_COLUMN`, `SOFT_DELETE_IS_FLAG`, `insert`, `update` and `id()` from the annotations above. It does
//! *not* generate `CreateDto`/`UpdateDto`/`ResponseDto` themselves — those
//! are ordinary structs the developer writes (or derives separately);
//! this macro only wires the type names together.
//!
//! This is a design-level sketch of the macro's shape and behavior; treat
//! it as a starting point to compile and iterate on inside the actual
//! crate, not as pre-verified output.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, parse_macro_input};

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
    let mut insert_idents = Vec::new(); // <--- these MUST exist on CreateDto
    let mut searchable = Vec::new();
    let mut sortable = Vec::new();
    let mut filterable = Vec::new();
    let mut soft_delete_col: Option<String> = None;
    let mut soft_delete_is_flag = false;

    let mut placeholders = Vec::new();
    let mut bind_idx = 1;
    let mut update_idents = Vec::new();
    for field in &fields.named {
        let ident = field.ident.clone().expect("named field");
        let col_name = ident.to_string();
        columns.push(col_name.clone());

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
            soft_delete_is_flag = matches!(
                unwrap_option(&field.ty).unwrap_or(&field.ty),
                syn::Type::Path(p) if p.path.is_ident("bool")
            );
        }

        // Only include in INSERT if it's not pk, not soft_delete, and not skipped for create
        if !attrs.pk && !attrs.soft_delete && !attrs.skip_create {
            insert_idents.push(ident.clone());
            placeholders.push(format!("${}", bind_idx));
            bind_idx += 1;
        }

        if !attrs.pk && !attrs.soft_delete && !attrs.skip_update {
            update_idents.push(ident.clone());
        }
    }

    let (pk_field, pk_ty) = match (pk_field, pk_ty) {
        (Some(f), Some(t)) => (f, t),
        _ => match fields
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
        },
    };

    let pk_column = pk_field.to_string();
    let table = container.table;
    let create_ty = container.create;

    let response_ty = container.response;

    let soft_delete_tokens = match soft_delete_col {
        Some(col) => quote! { Some(#col) },
        None => quote! { None },
    };

    let insert_cols: Vec<String> = insert_idents.iter().map(|i| i.to_string()).collect();
    let columns_str = insert_cols.join(", ");
    let placeholders_str = placeholders.join(", ");
    let insert_sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) RETURNING *",
        table, columns_str, placeholders_str
    );

    let select_all_sql = format!("SELECT * FROM {} WHERE {} = $1", table, pk_column);

    let (update_ty, update_impl) = match &container.update {
        Some(ty) => {
            // existing per-field codegen from last message: update_idents,
            // update_is_option, update_blocks, the QueryBuilder-based body.
            (
                quote! { #ty },
                quote! {
                    async fn update<'e, E>(
                    id: &Self::Id,
                    dto: Self::UpdateDto,
                    exec: E,
                ) -> ::core::result::Result<Self, ::sqlx::Error>
                where
                    E: ::sqlx::postgres::PgExecutor<'e> + Send,
                {
                    let mut qb: ::sqlx::QueryBuilder<::sqlx::Postgres> =
                        ::sqlx::QueryBuilder::new(concat!("UPDATE ", #table, " SET "));
                    let mut first = true;

                    #(
                        if let Some(v) = dto.#update_idents {
                            if !first { qb.push(", "); }
                            qb.push(concat!(stringify!(#update_idents), " = "));
                            qb.push_bind(v);
                            first = false;
                        }
                    )*

                    if first {
                        // Nothing to set — PATCH with no fields present is a no-op read.
                        return ::sqlx::query_as::<_, Self>(#select_all_sql)
                            .bind(id)
                            .fetch_one(exec)
                            .await;
                    }

                    qb.push(concat!(" WHERE ", #pk_column, " = "));
                    qb.push_bind(id.clone());
                    qb.push(" RETURNING *");
                    qb.build_query_as::<Self>().fetch_one(exec).await
                }
                },
            )
        }
        None => (
            quote! { ::viewset::NoUpdateDto },
            quote! {
                async fn update<'e, E>(
                    _id: &Self::Id,
                    dto: Self::UpdateDto,
                    _exec: E,
                ) -> ::core::result::Result<Self, ::sqlx::Error>
                where
                    E: ::sqlx::postgres::PgExecutor<'e> + Send,
                {
                    match dto {} // NoUpdateDto is uninhabited — exhaustive, zero arms
                }
            },
        ),
    };

    let expanded = quote! {
        impl ::viewset::Entity for #name {
            type Id = #pk_ty;
            type CreateDto = #create_ty;
            type UpdateDto = #update_ty;
            type ResponseDto = #response_ty;

            const TABLE: &'static str = #table;
            const PK_COLUMN: &'static str = #pk_column;
            const COLUMNS: &'static [&'static str] = &[#(#columns),*];
            const SEARCHABLE: &'static [&'static str] = &[#(#searchable),*];
            const SORTABLE: &'static [&'static str] = &[#(#sortable),*];
            const FILTERABLE: &'static [&'static str] = &[#(#filterable),*];
            const SOFT_DELETE_COLUMN: Option<&'static str> = #soft_delete_tokens;
            const SOFT_DELETE_IS_FLAG: bool = #soft_delete_is_flag;

            fn id(&self) -> Self::Id { self.#pk_field.clone() }

            async fn insert<'e, E>(dto: Self::CreateDto, exec: E) -> ::core::result::Result<Self, ::sqlx::Error>
        where
            E: ::sqlx::postgres::PgExecutor<'e> + Send,
        {
            ::sqlx::query_as::<_, Self>(#insert_sql)
                #(.bind(dto.#insert_idents))*
                .fetch_one(exec)
                .await
        }

         #update_impl
        }
    };
    expanded.into()
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
    update: Option<Ident>,
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
            update,
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
    skip_create: bool,
    skip_update: bool,
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
                } else if meta.path.is_ident("skip_create") {
                    out.skip_create = true;
                } else if meta.path.is_ident("skip_update") {
                    out.skip_update = true;
                }
                Ok(())
            });
        }
        out
    }
}
