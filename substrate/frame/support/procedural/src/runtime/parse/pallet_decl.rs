// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use quote::ToTokens;
use syn::{spanned::Spanned, Attribute, Ident, PathArguments};

/// The declaration of a pallet.
#[derive(Debug, Clone)]
pub struct PalletDeclaration {
	/// The name of the pallet, e.g.`System` in `System: frame_system`.
	pub name: Ident,
	/// Optional attributes tagged right above a pallet declaration.
	pub attrs: Vec<Attribute>,
	/// The path of the pallet, e.g. `frame_system` in `System: frame_system`.
	pub path: syn::Path,
	/// The instance of the pallet, e.g. `Instance1` in `Council: pallet_collective::<Instance1>`.
	pub instance: Option<Ident>,
}

impl PalletDeclaration {
	pub fn try_from(
		_attr_span: proc_macro2::Span,
		item: &syn::ItemType,
		path: &syn::TypePath,
	) -> syn::Result<Self> {
		let name = item.ident.clone();

		let mut path = path.path.clone();

		let mut instance = None;
		if let Some(segment) = path.segments.iter_mut().find(|seg| !seg.arguments.is_empty()) {
			if let PathArguments::AngleBracketed(syn::AngleBracketedGenericArguments {
				args, ..
			}) = segment.arguments.clone()
			{
				if let Some(syn::GenericArgument::Type(syn::Type::Path(arg_path))) = args.first() {
					instance =
						Some(Ident::new(&arg_path.to_token_stream().to_string(), arg_path.span()));
					segment.arguments = PathArguments::None;
				}
			}
		}

		Ok(Self { name, path, instance, attrs: item.attrs.clone() })
	}
}
