// A small fixture exercising the JavaScript symbol extractor.

function computeSubtotal(items) {
  return items.reduce((sum, item) => sum + item.price * item.qty, 0);
}

const applyDiscount = (subtotal, rate) => {
  return subtotal - subtotal * rate;
};

class ShoppingCart {
  constructor() {
    this.items = [];
  }

  add(item) {
    this.items.push(item);
  }

  total() {
    return computeSubtotal(this.items);
  }
}

export { ShoppingCart, computeSubtotal, applyDiscount };
