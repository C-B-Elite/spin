title = "SIP xxx - Application Configuration"
template = "main"
date = "2022-03-22T14:53:30Z"

---

Summary: A configuration system for Spin applications.

Owner: lann.martin@fermyon.com

Created: March 22, 2022

## Background

It is common for applications to require configuration at runtime that isn't known at build time or is too sensitive to be stored with build artifacts.

A few examples:

- Logging configuration
- Per-channel (production, staging, etc) service dependency URLs
- Database secrets

## Proposal

### Configuration is a tree, with the app at its root

e.g.:
- app_key
- component1
  - key1
  - key2
- component2
  - key3
  - nested-component
    - key4

### Each key has a "path" corresponding to its position in the tree

- e.g. `component2.nested-component.key4`

- Config keys should be restricted to e.g. `[a-z0-9_]` to allow path encoding / case folding

### Configuration "schema" is defined by the app and components

```toml
[config] # [application.config]?
key1 = "default_value"
# ...is equivalent to:
key1 = { default = "default_value" }

# "required" fields must be given a value
key2 = { required = true }

# "secret" field values should be handled with care (e.g. not logged)
key3 = { required = true, secret = true }

[[component]]
id = "component1"
...
# Reusable components would define their config in their own component manifest
[[component.config]]
key1 = "value"  # Path: component1.key1
```

### Configuration "providers" populate configuration

_I haven't thought this through too much. Would this live in `spin.toml`? How is config resolved across multiple providers?_
```toml
[[config_provider]]
type = "toml_file"
path = "config.toml"

[[config_provider]]
type = "environment"
# Some config might not be appropriate for some providers
exclude = "component2.*"
```

#### Example providers

- Environment provider
  - `component2.nested-component.key4` -> `SPIN_APP__COMPONENT2__NESTED_COMPONENT__KEY4`
  - 😬 _Boy howdy thats ugly! Better ideas?_
- File provider
  ```toml
  [component2.nested-component]
  key4 = "value"
  ```
- Vault provider

### Configuration is exposed via component interface

`spin-config.wit`
```
// Missing key is a runtime error
get_config: function(key: string) -> string
```
- Since each component gets its own instance of the `spin-config` import, the executor can resolve paths automatically and only expose a component's own config to it.

### Other design options

#### Application specifies component config
`spin.toml`
```toml
[[component]]
id = "component1"
[[component.config_values]]
key1 = "app-specific-value"
# references to app config keys (?)
key2 = "${app_key}"
```

#### Typed configuration

The above assumes only string values, but we could include some typing:
```toml
# Simple form is typed implicitly from its default value
string_key = "value"
string_key = { type = "string", default = "value" }

number_key = 123
number_key = { type = "number", default = 123 }

required_string = { type = "string", required = true }
# Would require e.g. base64 encoding in some places
encryption_key = { type = "bytes", required = true, secret = true}
```

- This would complicate everything for _us_ but is nicer for users.

#### WASI "configfs"

For languages without component support, we could expose config as synthetic mounted files:

```ruby
key1_value = File.read("/config/key1")

# Typed config
key1_value = JSON.parse(File.read("/config/key1.json"))

# "bytes" type
encryption_key = File.read("/config/encryption_key.raw")
```
  