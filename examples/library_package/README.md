# Library package

Start a new library with `cog init --lib integer-math`; this example shows the
resulting package layout and how a consumer imports its public module.

`src/lib.cog` is the library check entry point. The public `integer_math`
module contains the package API. Consumers include the package and import that
module separately with `@!include("arithmetic")` in the consumer entry point.

```sh
cog check examples/library_package
cog package build examples/library_package
cog run examples/library_package/consumer
```

To consume the archive instead of the source directory, change the dependency
in `consumer/dependencies.kdl` from `path=".."` to
`archive="../target/package-integer-math.cogpkg"`.
