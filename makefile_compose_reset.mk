# Makefile additions - Issue #361
# Add this to the main Makefile

.PHONY: compose-up compose-down compose-reset

compose-up:
	docker compose -f deploy/docker-compose.yml up -d

compose-down:
	docker compose -f deploy/docker-compose.yml down

compose-reset:
	@echo "Resetting Docker Compose stack..."
	@echo "⚠️  WARNING: This will delete all local data in volumes (database, etc.)"
	docker compose -f deploy/docker-compose.yml down -v
	@echo "✓ Volumes deleted"
	docker compose -f deploy/docker-compose.yml up -d
	@echo "✓ Stack brought back up with clean schema"
	@echo ""
	@echo "The local API and database are ready at:"
	@echo "  - API: http://localhost:8080"
	@echo "  - Postgres: localhost:5432"

# Local development helper
compose-logs:
	docker compose -f deploy/docker-compose.yml logs -f

compose-ps:
	docker compose -f deploy/docker-compose.yml ps
