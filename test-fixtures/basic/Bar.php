<?php

declare(strict_types=1);

namespace App;

use App\Test\Baz;

class Bar
{
    public function greet(string $name): string
    {
        (new Foo())->increment(5);

        $baz = new Baz();
        $baz->test();

        return "Hello, {$name}!";
    }
}