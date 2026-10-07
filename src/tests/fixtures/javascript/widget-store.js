// Fixture: a domain-neutral CommonJS module, which defines its API by
// assigning functions to members at the top level: a constructor's prototype,
// an exported object, `exports` itself.

"use strict";

var proto = (module.exports = {});

function WidgetStore(options) {
  this.options = options;
}

WidgetStore.prototype.render = function render(name) {
  return this.options[name];
};

proto.listen = function listen(port) {
  return port;
};

exports.createWidgetStore = function (options) {
  return new WidgetStore(options);
};

exports.WidgetStore = WidgetStore;

function setup() {
  proto.reset = () => {};
}

// chained and sequenced assignments define each target
exports.open = exports.show = function (name) {
  return name;
};
(exports.close = function () {}), (exports.flush = function () {});

function WidgetCache() {}

// an object assigned to the prototype declares the instance methods
WidgetCache.prototype = {
  get(key) {
    return key;
  },
  clear: function () {},
};

// none of these defines anything of the module's own
exports.default = function () {};
window.onload = function () {};
