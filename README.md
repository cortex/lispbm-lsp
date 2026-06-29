# lispBM LSP

This is a language server for lispBM.


This supports: going to definitions of symbols and getting doc comments from a the symbol on hover.

For this to work, each main lbm file needs an entry.toml file. This file defines the root of the import tree and what predefined symbols exists.

Doc comments are comments above a definition and/or on the same line after.

```lisp
; hello
(def hello 1)
```

```lisp
(def hello2 2) ; hello2
```

Multiple definitions of the same symbol will list them out during goto definition and merge the doc comment for hover.

```lisp
; hello
(def hello 1)
(def hello 2) ; hello again

(hello) ; <- hovering
```
Hovering over hello will get:


hello
__main.lisp__

hello again
__main.lisp__

---

Here is the format for the entry file:

## Entry point file format

- `entry_point`: path to main lispBM file, where all imports are done.
- `extensions`: List of extensions, each of which can be one of the following:
  - `path`: String to definition file, relative to the entry.toml file
  - `builtin`: String
  - `definition`: List of definitions with the same format as a definition file

### Definition file

- `definitions`: List of entries:
  - `symbol`: String
  - `comment`: String (optional)

### List of all builtins

-  lbm
- array
- crypt
- display
- dsp
- dyn
- lbm_image_format
- math
- mutex
- random
- runtime
- set
- string
- ttf
- vesc
- vesc_wifi
- vesc_ble

A list of each builtin is listed in the builtin folder

### EX

`entry.toml` file:
```toml
entry_point = "./main.lisp"

[[extension]]
path = "../../helpers.toml"

[[extension]]
builtin = "lbm"

[[extension]]
definitions = [
    {
       name = "hello",
       comment = "hello world function",
   },
]

```
`helpers.toml`:
```toml
definitions = [
    {
       name = "hello2",
       comment = "hello world function 2",
   },
]
```
