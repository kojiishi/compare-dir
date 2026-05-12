use std::fmt::Display;
use std::io::{self, Write};

pub(crate) struct ColumnFormatter {
    width: usize,
}

impl ColumnFormatter {
    pub fn new<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let width = iter
            .into_iter()
            .map(|s| s.as_ref().len())
            .max()
            .unwrap_or(0);
        Self { width }
    }

    pub fn write_value<W, S, V>(&self, writer: &mut W, name: S, value: V) -> io::Result<()>
    where
        W: Write,
        S: Display,
        V: Display,
    {
        writeln!(writer, "{:width$} {}", name, value, width = self.width)
    }

    pub fn write_values<W, I, S, V>(&self, writer: &mut W, iter: I) -> io::Result<()>
    where
        W: Write,
        I: IntoIterator<Item = (S, V)>,
        S: Display,
        V: Display,
    {
        for (name, value) in iter {
            self.write_value(writer, name, value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_formatter() -> anyhow::Result<()> {
        let labels = ["Short:", "VeryLongLabel:"];
        let formatter = ColumnFormatter::new(labels);
        assert_eq!(formatter.width, 14);

        let mut buf = Vec::new();
        formatter.write_value(&mut buf, "Short:", "Value1")?;
        formatter.write_value(&mut buf, "VeryLongLabel:", 42)?;
        let output = String::from_utf8(buf)?;
        assert_eq!(output, "Short:         Value1\nVeryLongLabel: 42\n");
        Ok(())
    }

    #[test]
    fn write_values() -> anyhow::Result<()> {
        let values = [("A:", 1), ("Longer:", 2)];
        let formatter = ColumnFormatter::new(values.iter().map(|(s, _)| *s));
        let mut buf = Vec::new();
        formatter.write_values(&mut buf, values)?;
        let output = String::from_utf8(buf)?;
        assert_eq!(output, "A:      1\nLonger: 2\n");
        Ok(())
    }
}
