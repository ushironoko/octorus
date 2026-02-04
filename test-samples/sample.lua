-- Lua sample file for tree-sitter syntax highlighting test
-- Phase 1 language support

local function fibonacci(n)
    if n <= 1 then
        return n
    end
    return fibonacci(n - 1) + fibonacci(n - 2)
end

-- Table and metatable example
local Vector = {}
Vector.__index = Vector

function Vector.new(x, y)
    local self = setmetatable({}, Vector)
    self.x = x or 0
    self.y = y or 0
    return self
end

function Vector:magnitude()
    return math.sqrt(self.x^2 + self.y^2)
end

function Vector:__add(other)
    return Vector.new(self.x + other.x, self.y + other.y)
end

-- Coroutine example
local producer = coroutine.create(function()
    for i = 1, 10 do
        coroutine.yield(i * 2)
    end
end)

-- Main execution
local v1 = Vector.new(3, 4)
local v2 = Vector.new(1, 2)
local v3 = v1 + v2

print("Fibonacci(10) =", fibonacci(10))
print("Vector magnitude:", v1:magnitude())
print("Vector sum:", v3.x, v3.y)

-- Table iteration
local fruits = {"apple", "banana", "cherry"}
for index, fruit in ipairs(fruits) do
    print(index, fruit)
end
