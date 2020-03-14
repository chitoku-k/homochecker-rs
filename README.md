# homochecker-rs
Rust implementation of [chitoku-k/HomoChecker](https://github.com/chitoku-k/HomoChecker) API.

* 🚀 **Blazingly Fast**
* 📦 **Easy to Use**
* 👨‍🔧 **High Torelance**

## Implemented APIs Overview
* Check API
    - `GET /check`
    - `GET /check/:user`
    - Query parameter
        - `format`: `sse` or `json` (optional, default to `sse`)
* List API
    - `GET /list`
    - `GET /list/:user`
    - Query parameter
        - `format`: `json` or `sql` (optional, default to `json`)
* Badge API
    - `GET /badge`

## Difference from chitoku-k/HomoChecker
* In API requests, trailing slashes are not accepted.
* RDBMS backend is PostgreSQL, not MySQL.
