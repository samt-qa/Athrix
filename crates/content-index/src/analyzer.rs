//! Adapter from the `athrix-text-ar` tokenizer to tantivy's `Tokenizer` trait.

use athrix_text_ar::{Analyzer, Tokens};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Name under which the analyzer is registered in every index.
pub const TOKENIZER_NAME: &str = "athrix_ar";

/// The Arabic analyzer as a tantivy tokenizer: tokens carry byte offsets in the original text.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArabicTokenizer(pub Analyzer);

impl Tokenizer for ArabicTokenizer {
    type TokenStream<'a> = ArabicTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> ArabicTokenStream<'a> {
        ArabicTokenStream {
            inner: self.0.tokens(text),
            buf: athrix_text_ar::Token::default(),
            token: Token::default(),
        }
    }
}

/// Token stream of [`ArabicTokenizer`].
#[derive(Debug)]
pub struct ArabicTokenStream<'a> {
    inner: Tokens<'a>,
    buf: athrix_text_ar::Token,
    token: Token,
}

impl TokenStream for ArabicTokenStream<'_> {
    fn advance(&mut self) -> bool {
        if !self.inner.advance(&mut self.buf) {
            return false;
        }
        let t = &mut self.token;
        t.text.clear();
        t.text.push_str(&self.buf.text);
        t.offset_from = self.buf.start;
        t.offset_to = self.buf.end;
        t.position = self.buf.position;
        t.position_length = 1;
        true
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::TextAnalyzer;

    #[test]
    fn tantivy_tokens_keep_original_offsets() {
        let mut a = TextAnalyzer::from(ArabicTokenizer::default());
        let text = "الْمُعَلِّمُونَ Reports";
        let mut s = a.token_stream(text);
        let mut got = Vec::new();
        while let Some(t) = s.next() {
            got.push((
                t.text.clone(),
                text[t.offset_from..t.offset_to].to_owned(),
                t.position,
            ));
        }
        assert_eq!(
            got,
            [
                ("معلم".to_owned(), "الْمُعَلِّمُونَ".to_owned(), 0),
                ("reports".to_owned(), "Reports".to_owned(), 1),
            ]
        );
    }
}
