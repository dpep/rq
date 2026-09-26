// Fixture: a small, domain-neutral JavaScript file — JSX in a `.jsx` file, a
// class with methods, both spellings of a function declaration, and a
// module-level const beside a require (an import, not a definition).

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

export const defaultAccount = buildAccount();

export function defaultAccountFor(label) {
  return label ? buildAccount() : defaultAccount;
}
