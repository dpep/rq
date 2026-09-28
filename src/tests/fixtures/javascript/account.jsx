// Fixture: a small, domain-neutral JavaScript file — JSX in a `.jsx` file, a
// class with methods, both spellings of a function declaration, a component
// wrapped in a call, and a module-level const beside a require (an import, not
// a definition).

const React = require("react");

export class Account {
  deposit(amount) {
    this.balance += amount;
    return this.balance;
  }
}

export function buildAccount() {
  return new Account();
}

export const AccountBadge = ({ label }) => <span>{label}</span>;

export const AccountRow = React.memo(({ account }) => <li>{account.id}</li>);

export const accountRowKey = (account) => `row-${account.id}`;

export const defaultAccount = buildAccount();

export function defaultAccountFor(label) {
  return label ? buildAccount() : defaultAccount;
}
