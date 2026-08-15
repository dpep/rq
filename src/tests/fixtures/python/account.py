"""Fixture: a small, domain-neutral Python file exercising class, method, and
free-function extraction."""


class Account:
    def deposit(self, amount):
        return amount

    def withdraw(self, amount):
        return amount


def build_account():
    return Account()
