"""A small fixture exercising the Python symbol extractor."""


class InventoryService:
    """Tracks stock levels for products."""

    def __init__(self, warehouse: str):
        self.warehouse = warehouse
        self._levels = {}

    def adjust(self, sku: str, delta: int) -> int:
        """Adjust the stock level for a SKU and return the new total."""
        self._levels[sku] = self._levels.get(sku, 0) + delta
        return self._levels[sku]


def restock(service: InventoryService, sku: str, amount: int) -> int:
    """Convenience function to add stock to a service."""
    return service.adjust(sku, amount)


async def fetch_remote_levels(warehouse: str) -> dict:
    """Asynchronously fetch levels for a warehouse (stubbed)."""
    return {"warehouse": warehouse, "levels": {}}
