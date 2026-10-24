# Hearts developer / operator tasks. Run `make help` for the list.

OWNER ?= gavinfancher
REGISTRY ?= ghcr.io/$(OWNER)
TAG ?= $(shell git rev-parse --short HEAD)
PLATFORM ?= linux/amd64
OVERLAY ?= deploy/overlays/homelab
NS ?= hearts
N ?= 5

.DEFAULT_GOAL := help
.PHONY: help dev dev-down test image push deploy undeploy monitoring status \
        logs-coordinator logs-worker psql scale-workers port-forward-grafana

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

dev: ## Run the full stack locally (postgres + coordinator + 2 workers) at http://localhost:8080
	docker compose up --build

dev-down: ## Stop the local stack and delete its database volume
	docker compose down -v

test: ## Format check, clippy, and tests
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

image: ## Build both images locally (amd64), tagged with TAG
	docker buildx build --platform $(PLATFORM) -f docker/Dockerfile --target coordinator -t $(REGISTRY)/hearts-coordinator:$(TAG) --load .
	docker buildx build --platform $(PLATFORM) -f docker/Dockerfile --target worker -t $(REGISTRY)/hearts-worker:$(TAG) --load .

push: ## Push both images to GHCR (needs `docker login ghcr.io`)
	docker push $(REGISTRY)/hearts-coordinator:$(TAG)
	docker push $(REGISTRY)/hearts-worker:$(TAG)

deploy: ## Apply the homelab overlay to the cluster
	kubectl apply -k $(OVERLAY)

undeploy: ## Delete everything the overlay created (the DB PVC is kept; delete it by hand)
	kubectl delete -k $(OVERLAY)

monitoring: ## Install kube-prometheus-stack and the Grafana dashboards
	helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
	helm repo update
	helm upgrade --install kube-prometheus-stack prometheus-community/kube-prometheus-stack \
	  -n monitoring --create-namespace -f deploy/monitoring/kube-prometheus-stack-values.yaml
	kubectl apply -k deploy/monitoring/dashboards

status: ## Pods (with nodes) and resource usage
	kubectl -n $(NS) get pods -o wide
	kubectl -n $(NS) top pods

logs-coordinator: ## Follow coordinator logs
	kubectl -n $(NS) logs -f deploy/hearts-coordinator

logs-worker: ## Follow logs of all workers
	kubectl -n $(NS) logs -f -l app.kubernetes.io/name=hearts-worker --prefix --max-log-requests=20

psql: ## Open psql inside the database pod
	kubectl -n $(NS) exec -it hearts-db-0 -c postgres -- sh -c 'psql -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'

scale-workers: ## Scale workers: make scale-workers N=5
	kubectl -n $(NS) scale deploy/hearts-worker --replicas=$(N)

port-forward-grafana: ## Grafana on http://localhost:3000
	kubectl -n monitoring port-forward svc/kube-prometheus-stack-grafana 3000:80
