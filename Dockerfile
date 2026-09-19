# Контейнерный режим демона.
#
# Демон управляет Docker'ом снаружи себя: внутрь пробрасывается `docker.sock`, а
# контейнеры игровых серверов создаются на хосте. Отсюда главная ловушка —
# `host_data_dir` в конфиге: `dockerd` резолвит bind-пути **на хосте**, и путь,
# видимый изнутри этого контейнера, даёт молча пустой маунт.

FROM rust:1.97.1-bookworm AS build
WORKDIR /src

# Сначала манифест: слой с зависимостями переживает правку исходников.
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && rm -rf src

COPY src ./src
# Пустышка выше уже дала `main.rs` этому же пути — без touch cargo считает
# бинарь свежим и не пересобирает настоящий код.
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
# ca-certificates — иначе демон не скачает ни ядро, ни образ по https.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/noro-noded /usr/local/bin/noro-noded

# 2022 — SFTP. HTTP-порт прямой передачи задаётся конфигом и пробрасывается по
# необходимости: за NAT он не нужен вовсе, файлы идут через мастер.
EXPOSE 2022

ENTRYPOINT ["/usr/local/bin/noro-noded"]
