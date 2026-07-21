.PHONY: hello_ll memory_fs run_memory_fs clean

hello_ll:
	cargo build --example hello_ll --features fuse_35

memory_fs:
	cargo build -p fuse3 --example memory_fs

run_memory_fs:
	mkdir -p /tmp/memfs
	cargo run -p fuse3 --example memory_fs -- /tmp/memfs

clean:
	cargo clean
