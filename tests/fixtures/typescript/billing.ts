// A small fixture exercising the TypeScript symbol extractor.

export interface Invoice {
  id: string;
  amount: number;
  status: InvoiceStatus;
}

export type InvoiceId = string;

export enum InvoiceStatus {
  Draft = "draft",
  Sent = "sent",
  Paid = "paid",
}

export function totalForInvoices(invoices: Invoice[]): number {
  return invoices.reduce((sum, inv) => sum + inv.amount, 0);
}

export class BillingEngine {
  private invoices: Invoice[] = [];

  record(invoice: Invoice): void {
    this.invoices.push(invoice);
  }

  outstanding(): number {
    return totalForInvoices(
      this.invoices.filter((i) => i.status !== InvoiceStatus.Paid),
    );
  }
}
