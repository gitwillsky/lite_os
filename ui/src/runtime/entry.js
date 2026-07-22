import React from "react";
import * as jsxRuntime from "react/jsx-runtime";
import { mount } from "./renderer.js";

globalThis.__liteReact = React;
globalThis.__liteJsxRuntime = jsxRuntime;
globalThis.__liteMount = mount;
