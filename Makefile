# Makefile for the fan controller.
#
#   make build     构建 release 版本并输出到项目根目录 ./fan
#   make install   安装 ./fan 到 /usr/local/bin/fan（安装前自动备份旧版本）
#
# install 需要 root 权限写入 /usr/local/bin，故安装步骤内联 sudo；
# build 始终以普通用户身份运行 cargo，避免 sudo make install 导致
# target/ 目录被 root 占用。

PREFIX  ?= /usr/local
BINDIR  := $(PREFIX)/bin
TARGET  := $(BINDIR)/fan
BACKUP  := $(BINDIR)/fan.bak
# cargo 实际产物目录：遵循 CARGO_TARGET_DIR（若设置），否则用项目 ./target
ifdef CARGO_TARGET_DIR
RELEASE := $(CARGO_TARGET_DIR)/release/fan
else
RELEASE := target/release/fan
endif

.PHONY: build install

# 构建最新的 release 二进制并放到项目根目录 ./fan
build:
	cargo build --release
	cp $(RELEASE) ./fan

# 安装 ./fan 到 $(TARGET)；若已存在旧版本则先备份到 $(BACKUP)
install: 
	@if [ -f $(TARGET) ]; then \
		echo "Backup: $(TARGET) -> $(BACKUP)"; \
		sudo cp -p $(TARGET) $(BACKUP); \
	else \
		echo "No existing $(TARGET), skip backup"; \
	fi
	@echo "Install: ./fan -> $(TARGET)"
	sudo install -m 755 ./fan $(TARGET)
	@echo "Done: $(TARGET) installed"
	@echo "Tip: sudo systemctl restart fan   # 若由 systemd 管理"
