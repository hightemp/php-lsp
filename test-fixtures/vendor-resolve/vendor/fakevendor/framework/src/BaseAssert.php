<?php

namespace FakeVendor\Framework;

/**
 * Base assertion class with helper methods.
 */
class BaseAssert
{
    /**
     * Create a stub for the given class.
     *
     * @param string $className
     * @return object
     */
    public function createStub(string $className): object
    {
        return new \stdClass();
    }

    /**
     * Assert that two values are equal.
     *
     * @param mixed $expected
     * @param mixed $actual
     * @return void
     */
    public static function assertEquals($expected, $actual): void
    {
    }
}
