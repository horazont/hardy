use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn instrument(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
