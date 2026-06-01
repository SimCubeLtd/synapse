// A small fixture exercising the TSX component extractor.
import React from "react";

export interface TenantSwitcherProps {
  tenants: string[];
  active: string;
  onSwitch: (tenant: string) => void;
}

export function TenantSwitcher({ tenants, active, onSwitch }: TenantSwitcherProps) {
  return (
    <div className="tenant-switcher">
      <label>Tenant</label>
      <select
        value={active}
        onChange={(e) => onSwitch(e.target.value)}
      >
        {tenants.map((tenant) => (
          <option key={tenant} value={tenant}>
            {tenant}
          </option>
        ))}
      </select>
    </div>
  );
}
