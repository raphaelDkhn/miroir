group "default" {
  targets = ["acme-crm-env", "mini-shop-env", "acme-apps-env", "miroir-sandbox"]
}

target "miroir" {
  context = "."
  tags    = ["miroir:dev"]
}

target "miroir-sandbox" {
  context  = "sandbox"
  args     = { MIROIR_IMAGE = "miroir:dev" }
  contexts = { "miroir:dev" = "target:miroir" }
  tags     = ["miroir-sandbox"]
  output   = ["type=docker"]
}

target "acme-crm" {
  context = "fixtures/acme-crm"
  tags    = ["acme-crm:dev"]
}

target "mini-shop" {
  context = "fixtures/mini-shop"
  tags    = ["mini-shop:dev"]
}

target "acme-crm-env" {
  context    = "fixtures/acme-crm"
  dockerfile = "Dockerfile.miroir"
  args       = { PRODUCT_IMAGE = "acme-crm:dev", MIROIR_IMAGE = "miroir:dev" }
  contexts   = { "acme-crm:dev" = "target:acme-crm", "miroir:dev" = "target:miroir" }
  tags       = ["acme-crm-env"]
  output     = ["type=docker"]
}

target "mini-shop-env" {
  context    = "fixtures/mini-shop"
  dockerfile = "Dockerfile.miroir"
  args       = { PRODUCT_IMAGE = "mini-shop:dev", MIROIR_IMAGE = "miroir:dev" }
  contexts   = { "mini-shop:dev" = "target:mini-shop", "miroir:dev" = "target:miroir" }
  tags       = ["mini-shop-env"]
  output     = ["type=docker"]
}

target "gitea-env" {
  context    = "examples/gitea"
  dockerfile = "Dockerfile.miroir"
  args       = { MIROIR_IMAGE = "miroir:dev" }
  contexts   = { "miroir:dev" = "target:miroir" }
  tags       = ["gitea-env"]
  output     = ["type=docker"]
}

target "acme-apps-env" {
  context    = "fixtures/acme-apps"
  dockerfile = "../acme-crm/Dockerfile.miroir"
  args       = { PRODUCT_IMAGE = "acme-crm:dev", MIROIR_IMAGE = "miroir:dev" }
  contexts   = { "acme-crm:dev" = "target:acme-crm", "miroir:dev" = "target:miroir" }
  tags       = ["acme-apps-env"]
  output     = ["type=docker"]
}
