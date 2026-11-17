# `docker buildx bake` builds the grader, the agent's container and the acme-crm world;
# `docker buildx bake gitea` builds the three Gitea worlds. Every image is loaded into Docker.
group "default" {
  targets = ["miroir", "main", "acme-crm-env"]
}

target "miroir" {
  context = "."
  tags    = ["miroir:dev"]
  output  = ["type=docker"]
}

target "main" {
  context    = "."
  dockerfile = "main.Dockerfile"
  tags       = ["miroir-main:dev"]
  output     = ["type=docker"]
}

target "acme-crm" {
  context = "fixtures/acme-crm"
  tags    = ["acme-crm:dev"]
  output  = ["type=docker"]
}

target "acme-crm-env" {
  context    = "fixtures/acme-crm"
  dockerfile = "Dockerfile.miroir"
  args       = { PRODUCT_IMAGE = "acme-crm:dev", MIROIR_IMAGE = "miroir:dev" }
  contexts   = { "acme-crm:dev" = "target:acme-crm", "miroir:dev" = "target:miroir" }
  tags       = ["acme-crm-env:dev"]
  output     = ["type=docker"]
}

target "gitea" {
  name       = "gitea-${world}"
  matrix     = { world = ["orbit", "ferry", "kestrel"] }
  context    = "examples/gitea"
  dockerfile = "Dockerfile.miroir"
  args       = { MIROIR_IMAGE = "miroir:dev", WORLD = world }
  contexts   = { "miroir:dev" = "target:miroir" }
  tags       = ["gitea-env:${world}"]
  output     = ["type=docker"]
}
