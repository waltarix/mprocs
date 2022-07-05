@_list:
	just -l

build:
	RUST_TARGET=x86_64-unknown-linux-musl make

test:
	cargo test -p mprocs
fmt:
	cargo fmt -p mprocs
