.PHONY: build install install-kresko test

FEATURES ?= default-release-binaries
OUTDIR ?= target/ubuntu
ROCKSDB_CXXFLAGS := -include cstdint

define zebra_cargo
	CXXFLAGS="$(strip $(CXXFLAGS) $(ROCKSDB_CXXFLAGS))" cargo $(1)
endef

build:
	$(call zebra_cargo,build --workspace --locked)

install:
	$(call zebra_cargo,install --locked zebrad)

install-kresko:
	@echo "==> Building zebrad for Ubuntu..."
	@mkdir -p $(OUTDIR)
	docker build \
		--build-arg FEATURES="$(FEATURES)" \
		--file docker/Dockerfile.kresko \
		--tag zebra-kresko \
		.
	docker create --name zebra-kresko-tmp zebra-kresko
	docker cp zebra-kresko-tmp:/build/target/release/zebrad $(OUTDIR)/zebrad
	docker rm zebra-kresko-tmp
	@echo "==> Ubuntu-compatible binary at $(OUTDIR)/zebrad"

test:
	$(call zebra_cargo,test --workspace)
