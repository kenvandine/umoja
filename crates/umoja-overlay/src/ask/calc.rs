//! Sums typed into Ask Umoja ("128*3+40", "2^10", "15% of 80"), answered
//! on the spot like GNOME's Calculator search provider, with no model.

/// The value of `text` if it is arithmetic: numbers, + - * / ^ ( ) and
/// "x% of y". Plain numbers and anything else give `None`, so "2024" or
/// "vlc" never turn into a calculator row.
pub fn evaluate(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches('=').trim();
    if t.is_empty() {
        return None;
    }
    // "15% of 80"
    if let Some((p, of)) = t.split_once("% of ") {
        let p: f64 = p.trim().parse().ok()?;
        let of = evaluate_expr(of.trim())?;
        return Some(p / 100.0 * of);
    }
    // A plain number ("2024", "-5") is not a sum.
    let body = t.strip_prefix('-').unwrap_or(t);
    if !body.chars().any(|c| "+-*/^×÷x".contains(c)) {
        return None;
    }
    evaluate_expr(t)
}

fn evaluate_expr(t: &str) -> Option<f64> {
    let tokens = tokenize(t)?;
    let mut p = Parser { tokens, pos: 0 };
    let v = p.sum()?;
    (p.pos == p.tokens.len() && v.is_finite()).then_some(v)
}

/// The result as people write it: no trailing zeros, at most 10 decimals.
pub fn format(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Num(f64),
    Op(char),
    Open,
    Close,
}

fn tokenize(t: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' => i += 1,
            '0'..='9' | '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                out.push(Tok::Num(s.parse().ok()?));
            }
            '+' | '-' | '*' | '/' | '^' | '%' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            'x' | '×' => {
                out.push(Tok::Op('*'));
                i += 1;
            }
            '÷' => {
                out.push(Tok::Op('/'));
                i += 1;
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<Tok> {
        self.tokens.get(self.pos).copied()
    }

    fn sum(&mut self) -> Option<f64> {
        let mut v = self.product()?;
        while let Some(Tok::Op(op @ ('+' | '-'))) = self.peek() {
            self.pos += 1;
            let r = self.product()?;
            v = if op == '+' { v + r } else { v - r };
        }
        Some(v)
    }

    fn product(&mut self) -> Option<f64> {
        let mut v = self.power()?;
        while let Some(Tok::Op(op @ ('*' | '/' | '%'))) = self.peek() {
            self.pos += 1;
            let r = self.power()?;
            v = match op {
                '*' => v * r,
                '/' => v / r,
                _ => v % r,
            };
        }
        Some(v)
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            // Right-associative: 2^3^2 = 2^9.
            let exp = self.power()?;
            return Some(base.powf(exp));
        }
        Some(base)
    }

    fn unary(&mut self) -> Option<f64> {
        match self.peek()? {
            Tok::Op('-') => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            Tok::Op('+') => {
                self.pos += 1;
                self.unary()
            }
            Tok::Num(n) => {
                self.pos += 1;
                Some(n)
            }
            Tok::Open => {
                self.pos += 1;
                let v = self.sum()?;
                (self.peek()? == Tok::Close).then_some(())?;
                self.pos += 1;
                Some(v)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums() {
        assert_eq!(evaluate("128*3+40"), Some(424.0));
        assert_eq!(evaluate("2^10"), Some(1024.0));
        assert_eq!(evaluate("2^3^2"), Some(512.0));
        assert_eq!(evaluate("(1 + 2) * 3"), Some(9.0));
        assert_eq!(evaluate("10 / 4 ="), Some(2.5));
        assert_eq!(evaluate("-3 + 5"), Some(2.0));
        assert_eq!(evaluate("6 x 7"), Some(42.0));
        assert_eq!(evaluate("15% of 80"), Some(12.0));
    }

    #[test]
    fn not_sums() {
        assert_eq!(evaluate("2024"), None);
        assert_eq!(evaluate("-5"), None);
        assert_eq!(evaluate("vlc"), None);
        assert_eq!(evaluate("turn on dark mode"), None);
        assert_eq!(evaluate("1/0"), None);
        assert_eq!(evaluate("(1+2"), None);
        assert_eq!(evaluate("4 + "), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format(424.0), "424");
        assert_eq!(format(2.5), "2.5");
        assert_eq!(format(1.0 / 3.0), "0.3333333333");
    }
}
