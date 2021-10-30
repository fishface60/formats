//! Support for deriving the `Decodable` and `Encodable` traits on enums for
//! the purposes of decoding/encoding ASN.1 `CHOICE` types as mapped to
//! enum variants.

use crate::{Asn1Type, FieldAttrs, TypeAttrs};
use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{DataEnum, Fields, FieldsUnnamed, Ident, Lifetime, Type, Variant};
use synstructure::{Structure, VariantInfo};

/// Registry of `CHOICE` alternatives for a given enum
type Alternatives = std::collections::BTreeMap<Asn1Type, Alternative>;

/// Derive the `Choice` trait for an enum.
pub(crate) struct DeriveChoice {
    /// `asn1` attributes defined at the type level.
    type_attrs: TypeAttrs,

    /// `CHOICE` alternatives for this enum.
    alternatives: Alternatives,

    /// Tags included in the impl body for `der::Choice`.
    choice_body: TokenStream,

    /// Enum match arms for the impl body for `TryFrom<der::asn1::Any<'_>>`.
    decode_body: TokenStream,

    /// Enum match arms for the impl body for `der::Encodable::encode`.
    encode_body: TokenStream,

    /// Enum match arms for the impl body for `der::Encodable::encoded_len`.
    encoded_len_body: TokenStream,
}

impl DeriveChoice {
    /// Derive `Decodable` on an enum.
    pub fn derive(s: Structure<'_>, data: &DataEnum, lifetime: Option<&Lifetime>) -> TokenStream {
        assert_eq!(
            s.variants().len(),
            data.variants.len(),
            "enum variant count mismatch"
        );

        let mut state = Self {
            type_attrs: TypeAttrs::parse(&s.ast().attrs),
            alternatives: Default::default(),
            choice_body: TokenStream::new(),
            decode_body: TokenStream::new(),
            encode_body: TokenStream::new(),
            encoded_len_body: TokenStream::new(),
        };

        for (variant_info, variant) in s.variants().iter().zip(&data.variants) {
            let field_attrs = FieldAttrs::parse(&variant.attrs);
            let asn1_type = field_attrs.asn1_type.unwrap_or_else(|| {
                panic!(
                    "no #[asn1(type=...)] specified for enum variant: {}",
                    variant.ident
                )
            });
            let tag = asn1_type.tag();

            Alternative::register(&mut state.alternatives, asn1_type, variant);
            state.derive_variant_choice(&tag);
            state.derive_variant_decoder(&tag, &field_attrs);

            match variant_info.bindings().len() {
                // TODO(tarcieri): handle 0 bindings for ASN.1 NULL
                1 => {
                    state.derive_variant_encoder(variant_info, &field_attrs);
                    state.derive_variant_encoded_len(variant_info);
                }
                other => panic!(
                    "unsupported number of ASN.1 variant bindings for {}: {}",
                    asn1_type, other
                ),
            }
        }

        state.finish(s, lifetime)
    }

    /// Derive the body of `Choice::can_decode
    fn derive_variant_choice(&mut self, tag: &TokenStream) {
        if self.choice_body.is_empty() {
            tag.clone()
        } else {
            quote!(| #tag)
        }
        .to_tokens(&mut self.choice_body);
    }

    /// Derive a match arm of the impl body for `TryFrom<der::asn1::Any<'_>>`.
    fn derive_variant_decoder(&mut self, tag: &TokenStream, field_attrs: &FieldAttrs) {
        let decoder = field_attrs.decoder(&self.type_attrs);
        { quote!(#tag => Ok(#decoder?.try_into()?),) }.to_tokens(&mut self.decode_body);
    }

    /// Derive a match arm for the impl body for `der::Encodable::encode`.
    fn derive_variant_encoder(&mut self, variant: &VariantInfo<'_>, field_attrs: &FieldAttrs) {
        assert_eq!(
            variant.bindings().len(),
            1,
            "unexpected number of variant bindings"
        );

        variant
            .each(|bi| {
                let binding = &bi.binding;
                let encoder_obj = field_attrs.encoder(&quote!(#binding), &self.type_attrs);
                quote!(#encoder_obj?.encode(encoder))
            })
            .to_tokens(&mut self.encode_body);
    }

    /// Derive a match arm for the impl body for `der::Encodable::encode`.
    fn derive_variant_encoded_len(&mut self, variant: &VariantInfo<'_>) {
        assert_eq!(
            variant.bindings().len(),
            1,
            "unexpected number of variant bindings"
        );

        variant
            .each(|bi| {
                let binding = &bi.binding;
                quote!(#binding.encoded_len())
            })
            .to_tokens(&mut self.encoded_len_body);
    }

    /// Finish deriving an enum
    fn finish(self, s: Structure<'_>, lifetime: Option<&Lifetime>) -> TokenStream {
        let lifetime = match lifetime {
            Some(lifetime) => quote!(#lifetime),
            None => quote!('_),
        };

        let Self {
            choice_body,
            decode_body,
            encode_body,
            encoded_len_body,
            ..
        } = self;

        let mut variant_conversions = TokenStream::new();

        for variant in self.alternatives.values() {
            let variant_ident = &variant.ident;
            let variant_type = &variant.field_type;

            variant_conversions.extend(s.gen_impl(quote! {
                gen impl From<#variant_type> for @Self {
                    fn from(field: #variant_type) -> Self {
                        Self::#variant_ident(field)
                    }
                }
            }));
        }

        s.gen_impl(quote! {
            gen impl ::der::Choice<#lifetime> for @Self {
                fn can_decode(tag: ::der::Tag) -> bool {
                    matches!(tag, #choice_body)
                }
            }

            gen impl ::der::Decodable<#lifetime> for @Self {
                fn decode(decoder: &mut ::der::Decoder<#lifetime>) -> ::der::Result<Self> {
                    match decoder.peek_tag()? {
                        #decode_body
                        actual => Err(der::ErrorKind::TagUnexpected {
                            expected: None,
                            actual
                        }
                        .into()),
                    }
                }
            }

            gen impl ::der::Encodable for @Self {
                fn encode(&self, encoder: &mut ::der::Encoder<'_>) -> ::der::Result<()> {
                    match self {
                        #encode_body
                    }
                }

                fn encoded_len(&self) -> ::der::Result<::der::Length> {
                    match self {
                        #encoded_len_body
                    }
                }
            }

            #variant_conversions
        })
    }
}

/// ASN.1 `CHOICE` alternative: one of the ASN.1 types comprising the `CHOICE`
/// which maps to an enum variant.
struct Alternative {
    /// [`Ident`] for the corresponding enum variant.
    pub ident: Ident,

    /// Type of the inner field (i.e. of the variant's 1-tuple)
    pub field_type: Type,
}

impl Alternative {
    /// Register a `CHOICE` alternative for a variant
    pub fn register(alternatives: &mut Alternatives, asn1_type: Asn1Type, variant: &Variant) {
        let field_type = match &variant.fields {
            Fields::Unnamed(FieldsUnnamed { unnamed, .. }) if unnamed.len() == 1 => {
                let field = unnamed.first().unwrap();
                field.ty.clone()
            }
            _ => panic!("can only derive `Choice` for enums with 1-tuple variants"),
        };

        let alternative = Self {
            ident: variant.ident.clone(),
            field_type,
        };

        if let Some(duplicate) = alternatives.insert(asn1_type, alternative) {
            panic!(
                "duplicate ASN.1 type `{}` for enum variants `{}` and `{}`",
                asn1_type, duplicate.ident, variant.ident
            );
        }
    }
}
