# Entry point file format

- `entry_point`: String
- `extensions`: List of extensions, each of which can be one of the following:
  - `path`: String
  - `builtin`: String
  - `definition`: List of String

# Directory index file format

- `definitions`: List of entries:
  - `symbol`: String
  - `comment`: String (optional)
  - `file`: String
  - `line`: Integer
  - `column`: Integer


# EX

```toml
entry_point = "./main.lisp"

[[extension]]
path = "../../helpers.toml"

[[extension]]
builtin = "core"

[[extension]]
definitions = [
    "hello"
]

```
