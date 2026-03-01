use proc_macro2::Ident;
use quote::{format_ident, quote};
use syn::{
    Fields, ItemStruct, Result, Token,
    parse::{Parse, ParseStream},
    parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
    token::Comma,
};

#[derive(Default)]
pub struct Args {
    deref_field: Option<Ident>,
}

// Support: #[smart_pointer(deref = field)], #[smart_pointer(deref(field))]
pub enum Arg {
    DerefEq(Ident),
    DerefParen(Ident),
}

impl Parse for Arg {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let key: Ident = input.parse()?; // expect `deref`
        if key != "deref" {
            return Err(syn::Error::new(key.span(), "unknown argument; expected `deref`"));
        }
        if input.peek(Token![=]) {
            let _: Token![=] = input.parse()?;
            let field: Ident = input.parse()?;
            Ok(Arg::DerefEq(field))
        } else if input.peek(syn::token::Paren) {
            let content;
            let _paren = syn::parenthesized!(content in input);
            let field: Ident = content.parse()?;
            Ok(Arg::DerefParen(field))
        } else {
            Err(syn::Error::new(key.span(), "expected `=` field_name or `(field_name)`"))
        }
    }
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Args::default());
        }
        let args: Punctuated<Arg, Comma> = input.parse_terminated(Arg::parse, Comma)?;
        let mut out = Args::default();
        for arg in args {
            match arg {
                Arg::DerefEq(id) | Arg::DerefParen(id) => {
                    if out.deref_field.is_some() {
                        return Err(syn::Error::new(id.span(), "duplicate `deref` argument"));
                    }
                    out.deref_field = Some(id);
                }
            }
        }
        Ok(out)
    }
}

pub fn expand_smart_pointer(orig: ItemStruct, args: Args) -> Result<proc_macro2::TokenStream> {
    // Only named-field structs.
    let fields_named = match &orig.fields {
        Fields::Named(named) => named,
        _ => {
            return Err(syn::Error::new(
                orig.span(),
                "#[smart_pointer] only supports structs with named fields",
            ));
        }
    };
    let fields_copy = fields_named.named.clone();

    let vis_wrapper = &orig.vis;
    let ident = &orig.ident;
    let data_ident = format_ident!("{ident}Data");
    let weak_ident = format_ident!("{ident}Ref");

    // Preserve generics
    let generics_for_header = orig.generics.clone();
    let params = generics_for_header.params.clone();
    let where_clause_hdr = generics_for_header.where_clause.clone();

    let generics_for_impl = orig.generics.clone();
    let (impl_generics, ty_generics, where_clause_impl) = generics_for_impl.split_for_impl();

    let vis_data: syn::Visibility = parse_quote!(pub);

    let data_deref_impl = if let Some(field_ident) = args.deref_field {
        let field =
            fields_named.named.iter().find(|f| f.ident.as_ref() == Some(&field_ident)).ok_or_else(
                || {
                    syn::Error::new(
                        field_ident.span(),
                        format!("`deref` field `{}` not found on struct", field_ident),
                    )
                },
            )?;
        let ty = &field.ty;
        Some(quote! {
            impl #impl_generics ::core::ops::Deref for #data_ident #ty_generics #where_clause_impl {
                type Target = #ty;
                #[inline]
                fn deref(&self) -> &Self::Target {
                    &self.#field_ident
                }
            }
        })
    } else {
        None
    };

    let out = quote! {
        // Wrapper tuple struct with full generics/bounds
        #vis_wrapper struct #ident <#params> (
            pub(crate) ::std::sync::Arc<#data_ident #ty_generics>
        ) #where_clause_hdr ;

        // Data struct with original fields, same generics/bounds
        #vis_data struct #data_ident <#params> #where_clause_hdr {
            #fields_copy
        }

        // Weak wrapper struct
        #vis_wrapper struct #weak_ident <#params> (
            pub(crate) ::std::sync::Weak<#data_ident #ty_generics>
        ) #where_clause_hdr ;

        // Deref to Data
        impl #impl_generics ::core::ops::Deref for #ident #ty_generics #where_clause_impl {
            type Target = #data_ident #ty_generics;
            #[inline]
            fn deref(&self) -> &Self::Target { &self.0 }
        }

        // Clone clones the Arc
        impl #impl_generics ::core::clone::Clone for #ident #ty_generics #where_clause_impl {
            #[inline]
            fn clone(&self) -> Self { Self(self.0.clone()) }
        }

        // PartialEq for strong wrapper
        impl #impl_generics ::core::cmp::PartialEq for #ident #ty_generics #where_clause_impl {
            #[inline]
            fn eq(&self, other: &Self) -> bool {
                ::std::sync::Arc::ptr_eq(&self.0, &other.0)
            }
        }

        // downgrade method on strong wrapper
        impl #impl_generics #ident #ty_generics #where_clause_impl {
            #[inline]
            pub fn downgrade(&self) -> #weak_ident #ty_generics {
                #weak_ident(::std::sync::Arc::downgrade(&self.0))
            }
        }

        // methods for Weak wrapper
        impl #impl_generics #weak_ident #ty_generics #where_clause_impl {
            #[inline]
            pub fn upgrade(&self) -> ::core::option::Option<#ident #ty_generics> {
                self.0.upgrade().map(#ident)
            }
        }

        // Default for Weak wrapper
        impl #impl_generics ::core::default::Default for #weak_ident #ty_generics #where_clause_impl {
            #[inline]
            fn default() -> Self { Self(::std::sync::Weak::default()) }
        }

        // Clone for Weak wrapper
        impl #impl_generics ::core::clone::Clone for #weak_ident #ty_generics #where_clause_impl {
            #[inline]
            fn clone(&self) -> Self { Self(self.0.clone()) }
        }

        // PartialEq for Weak wrapper
        impl #impl_generics ::core::cmp::PartialEq for #weak_ident #ty_generics #where_clause_impl {
            #[inline]
            fn eq(&self, other: &Self) -> bool {
                ::std::sync::Weak::ptr_eq(&self.0, &other.0)
            }
        }

        // Optional inner-field Deref on the Data struct
        #data_deref_impl
    };

    Ok(out)
}
