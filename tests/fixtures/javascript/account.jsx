// Fixture: a small, domain-neutral JavaScript file — JSX in a `.jsx` file, a
// class with methods, and both spellings of a function declaration.

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
