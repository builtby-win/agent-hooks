#!/usr/bin/env node
"use strict";
const { run } = require("./index.js");

run(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (error) => {
    console.error(error.message);
    process.exit(1);
  },
);
